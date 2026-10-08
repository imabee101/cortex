use agent_client_protocol as acp;

use cortex_login::{AuthManager, CortexAuth};

/// Require Cortex auth from a sync context: with no `.await` to refresh, a token inside the client's early-invalidation buffer still counts.
pub(crate) fn require_cortex_auth(
    auth_manager: &AuthManager,
    missing_message: &'static str,
    non_cortex_message: &'static str,
) -> Result<CortexAuth, acp::Error> {
    let auth = auth_manager
        .current_or_expired()
        .ok_or_else(|| acp::Error::auth_required().data(missing_message))?;
    if !auth.is_cortex_auth() {
        return Err(acp::Error::auth_required().data(non_cortex_message));
    }
    Ok(auth)
}
