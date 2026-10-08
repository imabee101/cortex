#[test]
fn startup_completed_carries_bootstrap_subphase_fields() {
    let home = tempfile::TempDir::new().expect("cortex home");
    // SAFETY: this binary has one test; no other thread reads the environment.
    unsafe { cortex_test_support::isolate_cortex_env(home.path()) };
    cortex_telemetry::unified_log::redirect_to_temp_for_tests();
    cortex_telemetry::startup::mark_process_start();
    let _timer = cortex_telemetry::startup::begin(cortex_telemetry::startup::Owner::Client);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let mut cfg = cortex_shell::agent::config::Config::default();
        cfg.remote_settings = Some(cortex_shell::util::config::RemoteSettings::default());
        let auth_manager = std::sync::Arc::new(cfg.create_auth_manager());
        cortex_shell::agent::init::bootstrap(&cfg, &auth_manager, None).expect("bootstrap");
        drop(cortex_shell::managed_config::take_refresh_supervisor());
    });

    cortex_telemetry::startup::PendingStartup::new()
        .finish(cortex_telemetry::startup::StartupOutcome::Ok);

    let log =
        String::from_utf8(cortex_telemetry::unified_log::snapshot_log().expect("unified log"))
            .expect("utf8");
    let ctx = log
        .lines()
        .find_map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).ok()?;
            if value.get("msg")?.as_str()? != cortex_telemetry::startup::STARTUP_COMPLETE_MSG {
                return None;
            }
            value.get("ctx").cloned()
        })
        .expect("startup complete record");

    for field in [
        "init_process_ms",
        "resolve_config_ms",
        "remote_settings_ms",
        "models_manager_ms",
    ] {
        assert!(
            ctx.get(field).and_then(serde_json::Value::as_u64).is_some(),
            "{field} must be populated on StartupCompleted, ctx={ctx}"
        );
    }
}
