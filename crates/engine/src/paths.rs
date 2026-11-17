//! Host path helpers kept after the git/repos removal (Phase 0): the home
//! directory, `~` expansion, and projectless chats' scratch folders.
//!
//! TODO(aurea-fase1): the folder-browser search helpers that lived in
//! `repos.rs` move here as the RPC surface is re-homed.

use std::path::PathBuf;

/// `ListFolders`/`SearchFiles` result caps.
pub(crate) const HOME_FOLDER_RESULTS: usize = 20;
pub(crate) const HOME_FOLDER_MAX_RESULTS: usize = 50;

/// Best-effort home directory (the `ListFolders` default and scratch base).
pub(crate) fn home_dir() -> PathBuf {
    let variables = if cfg!(windows) {
        ["USERPROFILE", "HOME"]
    } else {
        ["HOME", "USERPROFILE"]
    };
    variables
        .into_iter()
        .find_map(|name| {
            std::env::var_os(name)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// `~` / `~/…` → this host's home directory. Anything else passes through.
/// An unavailable home is an error so `~` never silently becomes `/`.
pub(crate) fn expand_home(cwd: &str) -> Result<String, &'static str> {
    match cwd.strip_prefix('~') {
        Some("") => Ok(session_home_dir()?.to_string_lossy().into_owned()),
        Some(rest) if rest.starts_with('/') => Ok(session_home_dir()?
            .join(&rest[1..])
            .to_string_lossy()
            .into_owned()),
        _ => Ok(cwd.to_string()),
    }
}

/// A chat cwd on this host: [`expand_home`], or a projectless chat's scratch
/// folder under `scratch_root`, created on first use — createChat only names
/// it, possibly on another device.
pub(crate) fn resolve_chat_cwd(cwd: &str, scratch_root: &std::path::Path) -> Result<String, String> {
    if !zeron_proto::projectless::is_scratch_cwd(cwd) {
        return expand_home(cwd).map_err(str::to_string);
    }
    let name = cwd.trim().rsplit('/').next().unwrap_or_default();
    let folder = scratch_root.join(name);
    std::fs::create_dir_all(&folder).map_err(|error| {
        format!(
            "could not create the chat's folder {}: {error}",
            folder.display()
        )
    })?;
    Ok(folder.to_string_lossy().into_owned())
}

/// Resolve the host user's home for a session that explicitly uses `~`.
/// An absent home must not silently turn a projectless session into `/`.
pub(crate) fn session_home_dir() -> Result<PathBuf, &'static str> {
    #[cfg(windows)]
    let names = ["USERPROFILE", "HOME"];
    #[cfg(not(windows))]
    let names = ["HOME", "USERPROFILE"];
    for name in names {
        let Some(value) = std::env::var_os(name).filter(|value| !value.is_empty()) else {
            continue;
        };
        let path = PathBuf::from(value);
        if path.is_absolute() && path.is_dir() && path.to_str().is_some() {
            return Ok(path);
        }
    }
    Err("User home directory unavailable on this device")
}

/// Where projectless chats' folders live on this host: the engine data dir's
/// `scratch` (`~/.zeron/scratch` for the default data dir).
pub(crate) fn scratch_root(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("scratch")
}
