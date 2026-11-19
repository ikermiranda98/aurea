//! What remains of the ACP adapters in Aurea: the helpers opencode and the
//! installer share (executable lookup, owned child processes). Aurea runs no
//! ACP agent, so nothing here installs or discovers one.

use std::path::PathBuf;

use zeron_proto::HarnessId;

use crate::HarnessError;

pub(crate) use crate::process::owned as child;

/// `exe` on `PATH` (and the extra directories), as opencode resolves its CLI.
pub(crate) fn find_on_paths(exe: &str, extra: Vec<PathBuf>) -> Option<PathBuf> {
    crate::executable::find_on_paths(exe, extra)
}

/// No ACP agent ships a pinned archive in Aurea.
pub fn can_install(_harness: HarnessId) -> bool {
    false
}

/// Archive installs were only for ACP agents, which Aurea removed.
pub async fn install_harness(harness: HarnessId) -> Result<(), HarnessError> {
    Err(HarnessError::NotInstalled(format!(
        "{harness:?} is not available in Aurea"
    )))
}

/// No prewarmed adapters remain.
pub fn prewarm_managed_adapters() {}

/// Antigravity is gone, so it adds no skill folders.
pub(crate) fn antigravity_skill_dirs() -> Vec<PathBuf> {
    Vec::new()
}
