//! The ACP agent registry is gone from Aurea: it runs opencode only. What
//! stays answers the engine's callers (updates, sign-in, RPC) with "none".

use std::path::Path;

use serde_json::Value;
use zeron_proto::{HarnessId, RegistryAgent};
use zeron_rpc::{RpcError, RpcReply, methods};

use crate::registry::HarnessRegistry;

/// Nothing to load: no registry agents are added in Aurea.
pub fn start(_data_dir: &Path, _registry: &HarnessRegistry) {}

/// No added agent comes from the registry.
pub(crate) fn from_registry(_agent: RegistryAgent) -> bool {
    false
}

pub(crate) async fn latest_version(_harness: HarnessId) -> Result<String, String> {
    Err("The agent registry isn't available in Aurea".into())
}

pub(crate) async fn install_latest(_agent: RegistryAgent, _expected: Option<&str>) -> Result<(), String> {
    Err("The agent registry isn't available in Aurea".into())
}

/// The registry's methods are still answered here, with a refusal.
pub(crate) fn handles(method: &str) -> bool {
    matches!(
        method,
        methods::LIST_ACP_REGISTRY
            | methods::ADD_ACP_AGENT
            | methods::REMOVE_ACP_AGENT
            | methods::GET_ACP_AGENT_ENV
            | methods::SET_ACP_AGENT_ENV
            | methods::GET_ACP_AGENT_SIGN_IN
            | methods::SIGN_OUT_ACP_AGENT
            | methods::SET_ACP_AGENT_KEY
            | methods::ADD_OWN_AGENT
    )
}

pub(crate) async fn dispatch(
    _registry: &HarnessRegistry,
    _updates: Option<&crate::harness_updates::HarnessUpdateCoordinator>,
    _method: &str,
    _params: Value,
) -> Result<RpcReply, RpcError> {
    Err(RpcError::Failed("The agent registry isn't available in Aurea".into()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn registry_methods_are_refused() {
        assert!(super::handles(zeron_rpc::methods::LIST_ACP_REGISTRY));
    }
}
