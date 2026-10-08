//! `CORTEX_HOME` override tests in an isolated binary so `cortex_home()`'s process-wide `OnceLock` initializes from the overridden env var.

use std::path::PathBuf;

#[test]
#[serial_test::serial(CORTEX_HOME)]
fn cortex_home_override_path_helpers() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cortex_home = tmp.path().to_path_buf();
    unsafe {
        std::env::set_var("CORTEX_HOME", &cortex_home);
    }

    assert_eq!(
        cortex_pager::util::pager_toml_path(),
        cortex_home.join("pager.toml")
    );
    assert_eq!(
        cortex_pager::util::display_cortex_home_prefix(),
        "$CORTEX_HOME"
    );
    assert_eq!(
        cortex_pager::util::display_user_cortex_path("config.toml"),
        "$CORTEX_HOME/config.toml"
    );

    let memory_path = cortex_home.join("memory/MEMORY.md");
    assert_eq!(
        cortex_pager::util::abbreviate_path(&memory_path.display().to_string()),
        "$CORTEX_HOME/memory/MEMORY.md"
    );

    // The copy toast abbreviates paths the same way, so a custom $CORTEX_HOME outside $HOME still shows the short form
    assert_eq!(
        cortex_pager::clipboard::display_copy_path(&cortex_home.join("last-copy.txt")),
        "$CORTEX_HOME/last-copy.txt"
    );

    assert!(cortex_pager::util::is_under_user_cortex_home(&memory_path));
    assert!(!cortex_pager::util::is_under_user_cortex_home(
        PathBuf::from("/tmp/other").as_path()
    ));
}

/// Isolated because `cortex_home()`'s `OnceLock` is already initialized by the time the shared lib-test binary reaches a case like this.
#[test]
#[serial_test::serial(CORTEX_HOME)]
fn disk_usage_run_creates_no_cortex_home() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ghost = tmp.path().join("ghost-home");
    unsafe {
        std::env::set_var("CORTEX_HOME", &ghost);
    }

    for json in [false, true] {
        cortex_pager::disk_usage_cmd::run(cortex_pager::disk_usage_cmd::DiskUsageArgs {
            json,
            clean: false,
            clean_orphaned: false,
            yes: false,
        })
        .expect("a missing home is not an error");
        assert!(
            !ghost.exists(),
            "cortex du must not create the home it reports on (json={json})"
        );
    }
}
