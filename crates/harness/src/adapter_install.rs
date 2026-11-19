//! Managed installs for npm-distributed ACP adapters.
//!
//! The old fallback spawned `npx -y <pkg>` at chat time, which put every
//! user's npm state in the hot path: a cold cache meant a multi-minute
//! download while the chat showed "Working", and a broken one meant npm dying
//! before the adapter ever ran — silently, with an errno-encoded exit code
//! (254 = ENOENT, the zeronsh/comet#95 crash) that surfaced as an opaque
//! "harness protocol error". Instead, pinned adapter packages are installed
//! ONCE into a zeron-owned prefix (`~/.zeron/adapters/<pkg>/<version>` on
//! Unix, the local app-data directory on Windows), with its own npm cache
//! beside it, so a root-owned or read-only user cache cannot break us. Every
//! subsequent launch spawns `node <entry>` directly — no npm anywhere near a
//! chat turn.
//!
//! Install is atomic: npm runs in a `.tmp-*` sibling which is renamed into
//! place only after the bin entry resolves and a marker file is written, so a
//! killed install can never masquerade as a working adapter, and concurrent
//! installers (two daemons, prewarm racing a run) converge on whichever
//! rename won.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;


pub(crate) const OK_MARKER: &str = ".zeron-install-ok";

/// Managed adapter storage. `$ZERON_ADAPTERS_DIR` wins, followed by
/// `$ZERON_DATA_DIR/adapters`. Windows defaults to
/// `%LOCALAPPDATA%/Zeron/adapters` (or `%USERPROFILE%/AppData/Local/...`);
/// Unix keeps `~/.zeron/adapters`.
pub(crate) fn adapters_root() -> Option<PathBuf> {
    adapters_root_with(
        &|key| std::env::var_os(key),
        crate::executable::Platform::current(),
    )
}

fn adapters_root_with(
    env: &impl Fn(&str) -> Option<OsString>,
    platform: crate::executable::Platform,
) -> Option<PathBuf> {
    let value = |key| {
        env(key)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };
    if let Some(dir) = value("ZERON_ADAPTERS_DIR") {
        return Some(dir);
    }
    if let Some(dir) = value("ZERON_DATA_DIR") {
        return Some(dir.join("adapters"));
    }
    if platform == crate::executable::Platform::Windows {
        value("LOCALAPPDATA")
            .map(|dir| dir.join("Zeron").join("adapters"))
            .or_else(|| {
                value("USERPROFILE").map(|home| {
                    home.join("AppData")
                        .join("Local")
                        .join("Zeron")
                        .join("adapters")
                })
            })
    } else {
        value("HOME").map(|home| home.join(".zeron").join("adapters"))
    }
}

fn npm_cli_for_node(node: &Path) -> Option<PathBuf> {
    let cli = node
        .parent()?
        .join("node_modules")
        .join("npm")
        .join("bin")
        .join("npm-cli.js");
    cli.is_file().then_some(cli)
}

fn find_npm_with(
    env: &impl Fn(&str) -> Option<OsString>,
    login_shell_path: Option<OsString>,
    platform: crate::executable::Platform,
) -> Option<PathBuf> {
    if platform == crate::executable::Platform::Windows {
        let node = crate::executable::find_on_paths_matching_with(
            "node",
            Vec::new(),
            env,
            login_shell_path,
            platform,
            |node| npm_cli_for_node(node).is_some(),
        )?;
        npm_cli_for_node(&node)
    } else {
        crate::executable::find_on_paths_with("npm", Vec::new(), env, login_shell_path, platform)
    }
}

pub(crate) fn find_npm() -> Option<PathBuf> {
    find_npm_with(
        &|key| std::env::var_os(key),
        crate::shell_env::user_path(),
        crate::executable::Platform::current(),
    )
}

/// One installer at a time per process; installs are rare and npm handles
/// its own intra-install parallelism.
pub(crate) fn install_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

const RENAME_RETRIES: u32 = 20;
const RENAME_RETRY_DELAY: Duration = Duration::from_millis(100);

/// Move a finished install into place. On Windows a just-written tree is
/// briefly held open by the malware scanner or by the verify process that
/// just exited, and the rename fails with a sharing error that clears on
/// its own: retry those for ~2s before reporting failure.
pub(crate) async fn rename_into_place(from: &Path, to: &Path) -> std::io::Result<()> {
    rename_retrying(from, to, crate::executable::Platform::current()).await
}

async fn rename_retrying(
    from: &Path,
    to: &Path,
    platform: crate::executable::Platform,
) -> std::io::Result<()> {
    let mut retries = 0;
    loop {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            // An existing destination also reads as access denied on
            // Windows; that is a lost race, not a held handle.
            Err(error)
                if platform == crate::executable::Platform::Windows
                    && retries < RENAME_RETRIES
                    && windows_handle_busy(&error)
                    && !to.exists() =>
            {
                retries += 1;
                tokio::time::sleep(RENAME_RETRY_DELAY).await;
            }
            Err(error) => return Err(error),
        }
    }
}

/// ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION: what
/// Windows reports while another process still has the file open.
pub(crate) fn windows_handle_busy(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(5 | 32 | 33))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;

    #[test]
    fn windows_sharing_errors_are_the_retryable_ones() {
        for code in [5, 32, 33] {
            assert!(windows_handle_busy(&std::io::Error::from_raw_os_error(
                code
            )));
        }
        // ERROR_FILE_NOT_FOUND / ERROR_PATH_NOT_FOUND / ERROR_ALREADY_EXISTS.
        for code in [2, 3, 183] {
            assert!(!windows_handle_busy(&std::io::Error::from_raw_os_error(
                code
            )));
        }
        assert!(!windows_handle_busy(&std::io::Error::other("busy")));
    }

    #[tokio::test]
    async fn install_rename_moves_into_place_and_fails_fast_when_not_busy() {
        let dir = tempfile::tempdir().unwrap();
        let (from, to) = (dir.path().join(".tmp-x"), dir.path().join("x"));
        std::fs::create_dir(&from).unwrap();
        rename_retrying(&from, &to, crate::executable::Platform::Windows)
            .await
            .unwrap();
        assert!(to.is_dir() && !from.exists());
        let started = std::time::Instant::now();
        assert!(
            rename_retrying(&from, &to, crate::executable::Platform::Windows)
                .await
                .is_err()
        );
        assert!(started.elapsed() < RENAME_RETRY_DELAY);
    }

    fn env(values: &[(&str, OsString)]) -> impl Fn(&str) -> Option<OsString> + use<> {
        let values: HashMap<String, OsString> = values
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect();
        move |key| values.get(key).cloned()
    }

    #[test]
    fn adapters_root_has_injected_windows_precedence_and_fallbacks() {
        let explicit = PathBuf::from(r"D:\Zeron adapters");
        let data = PathBuf::from(r"E:\Zeron data");
        let local = PathBuf::from(r"C:\Users\Ada\AppData\Local");
        let profile = PathBuf::from(r"C:\Users\Ada");
        assert_eq!(
            adapters_root_with(
                &env(&[
                    ("ZERON_ADAPTERS_DIR", explicit.clone().into_os_string()),
                    ("ZERON_DATA_DIR", data.clone().into_os_string()),
                    ("LOCALAPPDATA", local.clone().into_os_string()),
                ]),
                crate::executable::Platform::Windows,
            ),
            Some(explicit)
        );
        assert_eq!(
            adapters_root_with(
                &env(&[("ZERON_DATA_DIR", data.clone().into_os_string())]),
                crate::executable::Platform::Windows,
            ),
            Some(data.join("adapters"))
        );
        assert_eq!(
            adapters_root_with(
                &env(&[("LOCALAPPDATA", local.clone().into_os_string())]),
                crate::executable::Platform::Windows,
            ),
            Some(local.join("Zeron").join("adapters"))
        );
        assert_eq!(
            adapters_root_with(
                &env(&[("USERPROFILE", profile.clone().into_os_string())]),
                crate::executable::Platform::Windows,
            ),
            Some(
                profile
                    .join("AppData")
                    .join("Local")
                    .join("Zeron")
                    .join("adapters")
            )
        );
    }

}

