//! Signing in to an agent added from the ACP registry. Aurea has no registry
//! agents (it runs opencode only), so the sign-in refuses.

use zeron_proto::{AgentLoginStart, RegistryAgent};

use super::{AgentAccounts, EngineError};

impl AgentAccounts {
    pub(super) fn start_registry_login(
        &self,
        agent: RegistryAgent,
        _method: Option<&str>,
        _requester: Option<&str>,
    ) -> Result<AgentLoginStart, EngineError> {
        Err(EngineError::Other(format!(
            "{} isn't available in Aurea",
            agent.registry_id()
        )))
    }
}
