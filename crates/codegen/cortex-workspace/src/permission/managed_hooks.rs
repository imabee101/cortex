use std::path::Path;

use cortex_permission_rules::managed_policy::ManagedSettings;

/// The disabled-hooks file under `cortex_home` plus the `allow_managed_hooks_only` pin of `managed`.
pub fn disabled_hooks_snapshot(
    managed: &ManagedSettings,
    cortex_home: Option<&Path>,
) -> cortex_hooks::trust::DisabledHooks {
    cortex_hooks::trust::DisabledHooks::load(cortex_home, managed.non_managed_hooks.is_disabled())
}
