//! The commit scope a live settings fetch must still be in before it is cached or served.

use cortex_login::{AuthManager, CortexAuth, CortexComConfig};

/// Whether a managed-policy repair will run before this session starts.
/// While it returns true, no startup fetch may send an authenticated request.
pub type PolicyRepairPending = fn() -> bool;

/// How far a successful live fetch may be applied.
pub enum Commit {
    /// Origin, policy, and identity are all current.
    CacheAndServe,
    /// Origin and policy are current, but the identity is not saved to disk yet.
    /// The caller serves the value for this boot without writing the cache.
    ServeInMemory,
    /// A policy repair is still running.
    /// The caller retries on a later warm without recording a lasting failure.
    Retry,
    /// The origin or policy changed for good.
    Abandon,
}

/// The caller resolves `current_origin` (the proxy URL) and `alpha` with the same inputs the fetch used.
// TODO: newtype the origin (proxy or catalog) and identity strings.
// Comparing a proxy origin with a catalog origin should not compile.
pub(crate) fn evaluate_commit(
    current_origin: &str,
    expected_origin: &str,
    expected_identity: &str,
    auth_config: Option<&CortexComConfig>,
    alpha: Option<&str>,
    policy_repair_pending: PolicyRepairPending,
) -> Commit {
    if policy_repair_pending() {
        return Commit::Retry;
    }
    if !origin_still_current(current_origin, expected_origin) {
        return Commit::Abandon;
    }
    if identity_still_current(expected_identity, auth_config, alpha) {
        Commit::CacheAndServe
    } else {
        Commit::ServeInMemory
    }
}

fn origin_still_current(current_origin: &str, expected_origin: &str) -> bool {
    cortex_config::resolve_remote_fetch_enabled() && current_origin == expected_origin
}

/// Disk auth is resolved with the same `auth_config` the fetch used.
/// An empty `expected_identity` means the fetch was unauthenticated.
fn identity_still_current(
    expected_identity: &str,
    auth_config: Option<&CortexComConfig>,
    alpha: Option<&str>,
) -> bool {
    match resolve_disk_auth(auth_config.cloned()) {
        Some(auth) => crate::settings_cache_identity(&auth, alpha) == expected_identity,
        None => expected_identity.is_empty(),
    }
}

pub fn resolve_disk_auth(cortex_com_config: Option<CortexComConfig>) -> Option<CortexAuth> {
    let cortex_home = cortex_config::cortex_home();
    AuthManager::new_with_proxy_base_url(
        &cortex_home,
        cortex_com_config.unwrap_or_default(),
        cortex_config::EndpointsConfig::from_effective_config().proxy_url(),
    )
    .current()
}
