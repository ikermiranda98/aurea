//! The OpenCode CLI Aurea runs, fetched and owned by Aurea itself.
//!
//! Someone installing Aurea should not have to install Node, npm or a CLI
//! first. When no `opencode` resolves on this machine, Aurea downloads the
//! official release for its OS and architecture into the managed adapters
//! root (`~/.zeron/adapters/opencode/<version>`, `%LOCALAPPDATA%\Zeron\
//! adapters\...` on Windows) and runs that copy. `OPENCODE_EXECUTABLE` and a
//! CLI already on PATH still win: a person who keeps their own OpenCode keeps
//! using it.
//!
//! The release is pinned by version AND digest, so an install is exactly the
//! bytes OpenCode published. Bumping it is a code change: new `VERSION`, new
//! URLs, new SHA-512s (npm's `dist.integrity`, base64 decoded to hex).
//!
//! The packages are the ones OpenCode's own installer pulls
//! (`@opencode/cli-<os>-<arch>` from the npm registry); the binary sits at
//! `package/bin/opencode` (`opencode.exe` on Windows) inside the tarball.
//! GitHub's release assets are the OLD 1.x line — the desktop app and the
//! `opencode.ai` channel both ship 2.x, so the npm channel is the one that
//! matches the providers and models Aurea shows.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::HarnessError;
use crate::archive_install::{ArchivePin, Digest};

/// Directory name under the managed adapters root.
pub const NAME: &str = "opencode";
/// The release Aurea pins.
pub const VERSION: &str = "2.4.1";

const WINDOWS_X64: ArchivePin = ArchivePin {
    name: NAME,
    version: VERSION,
    url: "https://registry.npmjs.org/@opencode/cli-windows-x64/-/cli-windows-x64-2.4.1.tgz",
    entry: "package/bin/opencode.exe",
    digest: Digest::Sha512(
        "cd86f01fc69bdcbbf477ebb2bf0d29c6a4a37ff9bea113b11ced248c1043156bba658df56939d47228da3b111ced54ecb22c2493fd0721aa4000316e3ea66235",
    ),
};

const WINDOWS_ARM64: ArchivePin = ArchivePin {
    name: NAME,
    version: VERSION,
    url: "https://registry.npmjs.org/@opencode/cli-windows-arm64/-/cli-windows-arm64-2.4.1.tgz",
    entry: "package/bin/opencode.exe",
    digest: Digest::Sha512(
        "01be1d2d84d26b02c1d5c0c9b03542732dd5e43b631d903de4f10fbb4e3e613d6cd84787fd5b2350b6e6a9d456cf0191fda3d6b34f830388bd20fa8361f525e1",
    ),
};

const LINUX_X64: ArchivePin = ArchivePin {
    name: NAME,
    version: VERSION,
    url: "https://registry.npmjs.org/@opencode/cli-linux-x64/-/cli-linux-x64-2.4.1.tgz",
    entry: "package/bin/opencode",
    digest: Digest::Sha512(
        "1584009e779594b9aacbceb6ebbe2fbd15648fcc87b990051c9195dd4086431a3c68a1b9f081cf2abb296f1aee55a8e7204a5bd2df0f78eeb71ffa16e2f246e4",
    ),
};

const LINUX_ARM64: ArchivePin = ArchivePin {
    name: NAME,
    version: VERSION,
    url: "https://registry.npmjs.org/@opencode/cli-linux-arm64/-/cli-linux-arm64-2.4.1.tgz",
    entry: "package/bin/opencode",
    digest: Digest::Sha512(
        "a2534a7e802e1da0ed1eb5ab96ac61d913d2a2749f0193c6265aeeeed30d78d4e51f22d2ad36c37bff8727abdd611a7b838c8a2d243c486dbae14fc80bb3e0b7",
    ),
};

const DARWIN_X64: ArchivePin = ArchivePin {
    name: NAME,
    version: VERSION,
    url: "https://registry.npmjs.org/@opencode/cli-darwin-x64/-/cli-darwin-x64-2.4.1.tgz",
    entry: "package/bin/opencode",
    digest: Digest::Sha512(
        "27f9212e97b4a62b131830f98e975d423a9be5ef2a52c598a62464a8804dddafcfe12e5ab1f3f612b2ca49ebe6a46ce183d7c2d53a4b2671955f080df892602a",
    ),
};

const DARWIN_ARM64: ArchivePin = ArchivePin {
    name: NAME,
    version: VERSION,
    url: "https://registry.npmjs.org/@opencode/cli-darwin-arm64/-/cli-darwin-arm64-2.4.1.tgz",
    entry: "package/bin/opencode",
    digest: Digest::Sha512(
        "afa3c8665e70a85c5561f5458e6d892b8b9fd5add84fb14c1154ce0c917fd6129eeb005d2bb4c7cdb00cd524b2b91b8c9d29036fdf9d77cc4d3a4f7340aeadae",
    ),
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Os {
    Windows,
    Linux,
    Mac,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Arch {
    X64,
    Arm64,
}

fn pin_for(os: Os, arch: Arch) -> Option<ArchivePin> {
    Some(match (os, arch) {
        (Os::Windows, Arch::X64) => WINDOWS_X64,
        (Os::Windows, Arch::Arm64) => WINDOWS_ARM64,
        (Os::Linux, Arch::X64) => LINUX_X64,
        (Os::Linux, Arch::Arm64) => LINUX_ARM64,
        (Os::Mac, Arch::X64) => DARWIN_X64,
        (Os::Mac, Arch::Arm64) => DARWIN_ARM64,
    })
}

fn current_os() -> Os {
    if cfg!(windows) {
        Os::Windows
    } else if cfg!(target_os = "macos") {
        Os::Mac
    } else {
        Os::Linux
    }
}

fn current_arch() -> Arch {
    if cfg!(target_arch = "aarch64") {
        Arch::Arm64
    } else {
        Arch::X64
    }
}

/// The pinned release for this machine, `None` on a target OpenCode does not
/// build for (32-bit x86, riscv, …).
pub fn pin() -> Option<ArchivePin> {
    pin_for(current_os(), current_arch())
}

/// The managed CLI once its install completed, `None` otherwise.
pub fn installed() -> Option<PathBuf> {
    crate::archive_install::installed_entry(&pin()?)
}

/// Managed installs running in this process. A fresh machine lists harnesses
/// a moment after boot — before the download lands — and the harness must not
/// read as "not installed" for that whole session: the CLI is coming.
static INSTALLS: AtomicUsize = AtomicUsize::new(0);

/// Whether a managed install is in flight (or queued behind one).
pub fn install_in_progress() -> bool {
    INSTALLS.load(Ordering::Relaxed) > 0
}

/// Counts as in-flight until dropped, cancellation included.
pub(crate) struct Installing;

impl Installing {
    pub(crate) fn begin() -> Self {
        INSTALLS.fetch_add(1, Ordering::Relaxed);
        Self
    }
}

impl Drop for Installing {
    fn drop(&mut self) {
        INSTALLS.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Download, verify and unpack the pinned CLI unless that install exists.
/// Concurrent callers converge on one download (the install lock).
pub async fn ensure_installed() -> Result<PathBuf, HarnessError> {
    let pin = pin().ok_or_else(|| {
        HarnessError::Install(format!(
            "Aurea has no OpenCode build for {} {}; install the CLI yourself and point \
             OPENCODE_EXECUTABLE at it",
            std::env::consts::OS,
            std::env::consts::ARCH
        ))
    })?;
    let _installing = Installing::begin();
    crate::archive_install::ensure_installed(pin, "OpenCode").await
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [ArchivePin; 6] = [
        WINDOWS_X64,
        WINDOWS_ARM64,
        LINUX_X64,
        LINUX_ARM64,
        DARWIN_X64,
        DARWIN_ARM64,
    ];

    #[test]
    fn every_target_pins_its_own_archive_and_digest() {
        let mut urls = std::collections::BTreeSet::new();
        let mut digests = std::collections::BTreeSet::new();
        for pin in ALL {
            assert_eq!(pin.name, NAME);
            assert_eq!(pin.version, VERSION);
            assert!(pin.url.starts_with("https://registry.npmjs.org/@opencode/"));
            assert!(pin.url.contains(&format!("-{VERSION}.tgz")), "{}", pin.url);
            let hex = match pin.digest {
                Digest::Sha512(hex) => hex,
                Digest::Sha256(_) => panic!("{} must pin npm's SHA-512", pin.url),
            };
            assert_eq!(hex.len(), 128, "{}", pin.url);
            assert!(hex.chars().all(|c| c.is_ascii_hexdigit()), "{}", pin.url);
            assert!(urls.insert(pin.url), "duplicate url {}", pin.url);
            assert!(digests.insert(hex), "duplicate digest for {}", pin.url);
        }
    }

    #[test]
    fn windows_ships_an_exe_and_unix_platforms_a_bare_binary() {
        assert_eq!(
            pin_for(Os::Windows, Arch::X64).unwrap().entry,
            "package/bin/opencode.exe"
        );
        assert_eq!(
            pin_for(Os::Windows, Arch::Arm64).unwrap().entry,
            "package/bin/opencode.exe"
        );
        for os in [Os::Linux, Os::Mac] {
            for arch in [Arch::X64, Arch::Arm64] {
                assert_eq!(pin_for(os, arch).unwrap().entry, "package/bin/opencode");
            }
        }
    }

    #[test]
    fn an_in_flight_install_reads_as_coming_until_it_drops() {
        assert!(!install_in_progress());
        let first = Installing::begin();
        let second = Installing::begin();
        assert!(install_in_progress());
        drop(first);
        assert!(install_in_progress(), "one install still running");
        drop(second);
        assert!(!install_in_progress());
    }

    #[test]
    fn the_machine_picks_an_archive_that_matches_it() {
        let pin = pin().expect("every supported CI target has a pin");
        assert_eq!(
            pin.entry,
            if cfg!(windows) {
                "package/bin/opencode.exe"
            } else {
                "package/bin/opencode"
            }
        );
        assert_eq!(
            pin.url.contains("windows-x64"),
            cfg!(all(windows, target_arch = "x86_64"))
        );
    }
}
