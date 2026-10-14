use std::path::Path;
use std::path::PathBuf;

use cortex_hooks::discovery::ClaudeImport;
use cortex_hooks::trust::Trust;

use super::PluginConfigInputs;
use super::resolve_effective_plugins_config;

fn inputs<'a>(cwd: &'a Path, effective_config: Option<&'a toml::Value>) -> PluginConfigInputs<'a> {
    PluginConfigInputs {
        effective_config,
        home: None,
        cortex_home: None,
        cwd,
        trust: Trust::Untrusted,
        claude_import: ClaudeImport::Imported,
    }
}

#[test]
fn reads_claude_settings_plugins_only_when_opted_in_and_not_imported() {
    let home = tempfile::tempdir().expect("create temp home");
    let cwd = tempfile::tempdir().expect("create temp cwd");

    let claude = home.path().join(".claude");
    std::fs::create_dir_all(&claude).expect("create .claude");
    std::fs::write(
        claude.join("settings.json"),
        r#"{"enabledPlugins":{"cutoff-probe":true}}"#,
    )
    .expect("write .claude/settings.json");

    let opted_in: toml::Value =
        toml::from_str("[compat.claude]\nplugins = true\n").expect("parse config");
    for (effective_config, claude_import, expected) in [
        (None, ClaudeImport::NotImported, false),
        (Some(&opted_in), ClaudeImport::Imported, false),
        (Some(&opted_in), ClaudeImport::NotImported, true),
    ] {
        let config = resolve_effective_plugins_config(PluginConfigInputs {
            home: Some(home.path()),
            claude_import,
            ..inputs(cwd.path(), effective_config)
        });
        let label = format!(
            "opted in: {}, {claude_import:?}",
            effective_config.is_some()
        );
        assert_eq!(effective_config.is_some(), config.claude, "{label}");
        assert_eq!(
            expected,
            config.enabled.iter().any(|name| name == "cutoff-probe"),
            "{label}"
        );
    }
}

#[test]
fn takes_plugins_from_the_passed_effective_config() {
    let cwd = tempfile::tempdir().expect("create temp cwd");
    let effective_config: toml::Value =
        toml::from_str("[plugins]\nenabled = [\"campaign-plugin\"]\n").expect("parse config");

    let config = resolve_effective_plugins_config(inputs(cwd.path(), Some(&effective_config)));

    assert_eq!(vec!["campaign-plugin".to_owned()], config.enabled);
}

#[test]
fn a_malformed_plugins_list_keeps_the_disabled_list() {
    let cwd = tempfile::tempdir().expect("create temp cwd");
    let effective_config: toml::Value =
        toml::from_str("[plugins]\npaths = \"not-a-list\"\ndisabled = [\"kept\"]\n")
            .expect("parse config");

    let config = resolve_effective_plugins_config(inputs(cwd.path(), Some(&effective_config)));

    assert_eq!(vec!["kept".to_owned()], config.disabled);
}

#[test]
fn project_disabled_merges_untrusted_and_project_paths_only_when_trusted() {
    let cwd = tempfile::tempdir().expect("create temp cwd");
    let cortex = cwd.path().join(".cortex");
    std::fs::create_dir_all(&cortex).expect("create .cortex");
    std::fs::write(
        cortex.join("config.toml"),
        "[plugins]\npaths = [\"./project-plugin\"]\ndisabled = [\"project-off\"]\n",
    )
    .expect("write .cortex/config.toml");

    for (trust, expected_paths) in [
        (Trust::Untrusted, Vec::new()),
        (Trust::Trusted, vec![PathBuf::from("./project-plugin")]),
    ] {
        let config = resolve_effective_plugins_config(PluginConfigInputs {
            trust,
            ..inputs(cwd.path(), None)
        });
        assert_eq!(
            (expected_paths, vec!["project-off".to_owned()]),
            (config.config_paths, config.disabled),
            "{trust:?}"
        );
    }
}
