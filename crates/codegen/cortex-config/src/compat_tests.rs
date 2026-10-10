use super::*;

fn env_with(set: &[(&str, bool)]) -> CompatEnv {
    CompatEnv::read(|name| {
        set.iter()
            .find_map(|(variable, value)| (*variable == name).then_some(*value))
    })
}

fn parse_compat(source: &str) -> CompatConfigToml {
    let config: toml::Value = toml::from_str(source).unwrap();
    config.get("compat").unwrap().clone().try_into().unwrap()
}

fn assert_session_only_enabled(config: CompatConfig, expected: CompatVendor) {
    for cell in COMPAT_CELLS {
        if cell.surface() == CompatSurface::Sessions {
            assert_eq!(
                cell.vendor() == expected,
                config.value(cell),
                "{}.sessions",
                Into::<&'static str>::into(cell.vendor())
            );
        }
    }
}

fn remote_settings_with(key: CompatRemoteKey, value: bool) -> RemoteSettings {
    let mut remote = RemoteSettings::default();
    match key {
        CompatRemoteKey::CursorSkills => remote.cursor_skills_enabled = Some(value),
        CompatRemoteKey::CursorRules => remote.cursor_rules_enabled = Some(value),
        CompatRemoteKey::CursorAgents => remote.cursor_agents_enabled = Some(value),
        CompatRemoteKey::CursorMcps => remote.cursor_mcps_enabled = Some(value),
        CompatRemoteKey::CursorHooks => remote.cursor_hooks_enabled = Some(value),
        CompatRemoteKey::CursorSessions => remote.cursor_sessions_enabled = Some(value),
        CompatRemoteKey::ClaudeSkills => remote.claude_skills_enabled = Some(value),
        CompatRemoteKey::ClaudeRules => remote.claude_rules_enabled = Some(value),
        CompatRemoteKey::ClaudeAgents => remote.claude_agents_enabled = Some(value),
        CompatRemoteKey::ClaudeMcps => remote.claude_mcps_enabled = Some(value),
        CompatRemoteKey::ClaudeHooks => remote.claude_hooks_enabled = Some(value),
        CompatRemoteKey::ClaudeSessions => remote.claude_sessions_enabled = Some(value),
        CompatRemoteKey::CodexSessions => remote.codex_sessions_enabled = Some(value),
    }
    remote
}

#[test]
fn resolve_compat_defaults_match_registry() {
    assert_eq!(
        CompatConfig::default(),
        resolve_compat_config(&CompatConfigToml::default(), &CompatEnv::default(), None)
    );
}

#[test]
fn resolve_compat_toml_sessions_enable_independently() {
    for (vendor, section) in [
        (CompatVendor::Cursor, "cursor"),
        (CompatVendor::Claude, "claude"),
        (CompatVendor::Codex, "codex"),
    ] {
        let config = parse_compat(&format!("[compat.{section}]\nsessions = true"));
        assert_session_only_enabled(
            resolve_compat_config(&config, &CompatEnv::default(), None),
            vendor,
        );
    }
}

#[test]
fn resolve_compat_sessions_fails_closed_per_vendor() {
    let effective_config: toml::Value = toml::from_str(
        r#"
[compat.cursor]
sessions = "malformed"
[compat.claude]
sessions = false
[compat.codex]
sessions = true
hooks = "unrelated malformed field"
"#,
    )
    .unwrap();

    let resolved = resolve_compat_sessions(Some(&effective_config), &CompatEnv::default(), None);

    assert_eq!(
        CompatSessions {
            cursor: false,
            claude: false,
            codex: true,
        },
        resolved
    );
}

#[test]
fn resolve_compat_sessions_keeps_absent_and_valid_cells_independent() {
    let effective_config: toml::Value = toml::from_str(
        r#"
[compat.cursor]
sessions = false
hooks = "malformed but irrelevant"
[compat.claude]
sessions = true
"#,
    )
    .unwrap();
    let remote = RemoteSettings {
        codex_sessions_enabled: Some(false),
        ..RemoteSettings::default()
    };

    let resolved = resolve_compat_sessions(
        Some(&effective_config),
        &CompatEnv::default(),
        Some(&remote),
    );

    assert_eq!(
        CompatSessions {
            cursor: false,
            claude: true,
            codex: false,
        },
        resolved
    );
}

#[test]
fn resolve_compat_hooks_treats_an_unreadable_cell_as_unset() {
    let cursor_on: toml::Value = toml::from_str("[compat.cursor]\nhooks = true\n").unwrap();
    let claude_malformed: toml::Value =
        toml::from_str("[compat.claude]\nhooks = \"malformed\"\n").unwrap();
    let remote_claude_on = remote_settings_with(CompatRemoteKey::ClaudeHooks, true);
    let cases = [
        (
            "valid cell",
            Some(&cursor_on),
            None,
            CompatHooks {
                cursor: true,
                claude: false,
            },
        ),
        (
            "malformed cell without remote",
            Some(&claude_malformed),
            None,
            CompatHooks {
                cursor: false,
                claude: false,
            },
        ),
        (
            "malformed cell with remote",
            Some(&claude_malformed),
            Some(&remote_claude_on),
            CompatHooks {
                cursor: false,
                claude: true,
            },
        ),
        (
            "unreadable config with remote",
            None,
            Some(&remote_claude_on),
            CompatHooks {
                cursor: false,
                claude: true,
            },
        ),
    ];
    for (label, effective_config, remote, expected) in cases {
        assert_eq!(
            expected,
            resolve_compat_hooks(effective_config, &CompatEnv::default(), remote),
            "{label}"
        );
    }
}

#[test]
fn compat_config_cell_is_tolerant_and_fail_closed_per_cell() {
    let effective_config: toml::Value = toml::from_str(
        r#"
[compat.cursor]
skills = false
rules = "malformed"
[compat.claude]
hooks = true
"#,
    )
    .unwrap();
    let cell = |vendor, surface| {
        COMPAT_CELLS
            .into_iter()
            .find(|cell| cell.vendor() == vendor && cell.surface() == surface)
            .unwrap()
    };

    assert_eq!(
        Ok(Some(false)),
        compat_config_cell(
            Some(&effective_config),
            cell(CompatVendor::Cursor, CompatSurface::Skills)
        )
    );
    assert_eq!(
        Err(CompatConfigCellError::Malformed),
        compat_config_cell(
            Some(&effective_config),
            cell(CompatVendor::Cursor, CompatSurface::Rules)
        )
    );
    assert_eq!(
        Ok(Some(true)),
        compat_config_cell(
            Some(&effective_config),
            cell(CompatVendor::Claude, CompatSurface::Hooks)
        )
    );
    assert_eq!(
        Ok(None),
        compat_config_cell(
            Some(&effective_config),
            cell(CompatVendor::Codex, CompatSurface::Sessions)
        )
    );
    assert_eq!(
        Err(CompatConfigCellError::Unavailable),
        compat_config_cell(None, cell(CompatVendor::Claude, CompatSurface::Sessions))
    );
}

#[test]
fn resolve_compat_sessions_load_failure_fails_closed() {
    let resolved = resolve_compat_sessions(None, &CompatEnv::default(), None);

    assert_eq!(
        CompatSessions {
            cursor: false,
            claude: false,
            codex: false,
        },
        resolved
    );
}

#[test]
fn resolve_compat_sessions_load_failure_allows_env_override() {
    let env = env_with(&[("CORTEX_CODEX_SESSIONS_ENABLED", true)]);

    let resolved = resolve_compat_sessions(None, &env, None);

    assert_eq!(
        CompatSessions {
            cursor: false,
            claude: false,
            codex: true,
        },
        resolved
    );
}

#[test]
fn resolve_compat_sessions_valid_empty_uses_remote_and_defaults() {
    let effective_config = toml::Value::Table(toml::map::Map::new());
    let remote = RemoteSettings {
        claude_sessions_enabled: Some(true),
        ..RemoteSettings::default()
    };

    let resolved = resolve_compat_sessions(
        Some(&effective_config),
        &CompatEnv::default(),
        Some(&remote),
    );

    assert_eq!(
        CompatSessions {
            cursor: false,
            claude: true,
            codex: false,
        },
        resolved
    );
}

#[test]
fn remote_keys_are_one_hot_and_override_the_default() {
    for key in COMPAT_CELLS
        .into_iter()
        .filter_map(|cell| cell.remote_key())
    {
        let remote = remote_settings_with(key, false);
        for cell in COMPAT_CELLS {
            assert_eq!(
                (cell.remote_key() == Some(key)).then_some(false),
                remote_compat_value(Some(&remote), cell.remote_key()),
                "{key:?} mapped to {}.{}",
                Into::<&'static str>::into(cell.vendor()),
                Into::<&'static str>::into(cell.surface())
            );
        }
    }

    let remote = remote_settings_with(CompatRemoteKey::CursorSkills, true);
    assert!(!CompatConfig::default().cursor.skills);
    assert!(
        resolve_compat_config(
            &CompatConfigToml::default(),
            &CompatEnv::default(),
            Some(&remote)
        )
        .cursor
        .skills
    );
}

#[test]
fn resolve_compat_env_sessions_enable_independently() {
    for (vendor, env_var) in [
        (CompatVendor::Cursor, "CORTEX_CURSOR_SESSIONS_ENABLED"),
        (CompatVendor::Claude, "CORTEX_CLAUDE_SESSIONS_ENABLED"),
        (CompatVendor::Codex, "CORTEX_CODEX_SESSIONS_ENABLED"),
    ] {
        assert_session_only_enabled(
            resolve_compat_config(
                &CompatConfigToml::default(),
                &env_with(&[(env_var, true)]),
                None,
            ),
            vendor,
        );
    }
}

#[test]
fn resolve_compat_precedence_and_reserved_codex_hook() {
    let config = parse_compat("[compat.cursor]\nsessions = false\n[compat.codex]\nhooks = false");
    let remote = RemoteSettings {
        cursor_sessions_enabled: Some(true),
        ..RemoteSettings::default()
    };

    let resolved = resolve_compat_config(&config, &CompatEnv::default(), Some(&remote));
    assert!(!resolved.cursor.sessions);
    assert!(!resolved.codex.hooks);
    assert!(
        !resolved.cursor.hooks && !resolved.claude.hooks,
        "unset cells keep the default"
    );

    let env = env_with(&[
        ("CORTEX_CURSOR_SESSIONS_ENABLED", true),
        ("CORTEX_CODEX_HOOKS_ENABLED", true),
    ]);
    let resolved = resolve_compat_config(&config, &env, Some(&remote));
    assert!(resolved.cursor.sessions);
    assert!(resolved.codex.hooks);
}

#[test]
fn registry_and_defaults_cover_every_cell() {
    use CompatRemoteKey::*;

    assert_eq!(
        COMPAT_CELLS.map(|cell| {
            (
                Into::<&'static str>::into(cell.vendor()),
                Into::<&'static str>::into(cell.surface()),
                cell.remote_key(),
            )
        }),
        [
            ("cursor", "skills", Some(CursorSkills)),
            ("cursor", "rules", Some(CursorRules)),
            ("cursor", "agents", Some(CursorAgents)),
            ("cursor", "mcps", Some(CursorMcps)),
            ("cursor", "hooks", Some(CursorHooks)),
            ("cursor", "sessions", Some(CursorSessions)),
            ("claude", "skills", Some(ClaudeSkills)),
            ("claude", "rules", Some(ClaudeRules)),
            ("claude", "agents", Some(ClaudeAgents)),
            ("claude", "mcps", Some(ClaudeMcps)),
            ("claude", "hooks", Some(ClaudeHooks)),
            ("claude", "sessions", Some(ClaudeSessions)),
            ("claude", "settings", None),
            ("claude", "plugins", None),
            ("codex", "skills", None),
            ("codex", "rules", None),
            ("codex", "agents", None),
            ("codex", "mcps", None),
            ("codex", "hooks", None),
            ("codex", "sessions", Some(CodexSessions)),
        ]
    );

    // Nothing from another harness loads unless the user turns its cell on.
    let defaults = CompatConfig::default();
    let all = CompatConfig::all_enabled();
    for cell in COMPAT_CELLS {
        let name = format!(
            "{}.{}",
            Into::<&'static str>::into(cell.vendor()),
            Into::<&'static str>::into(cell.surface())
        );
        assert!(!defaults.value(cell), "{name} defaults on");
        assert!(all.value(cell), "{name} missing from all_enabled");
    }
    for vendor in [defaults.cursor, defaults.claude, defaults.codex] {
        assert_eq!(vendor, VendorCompat::default());
        assert!(!(vendor.skills || vendor.rules || vendor.agents || vendor.mcps));
        assert!(!(vendor.hooks || vendor.sessions || vendor.settings || vendor.plugins));
    }
    assert_eq!(defaults.skill_config_dirs(), [".cortex", ".agents"]);
    assert_eq!(defaults.rules_dirs(), [".cortex/rules"]);
    assert!(defaults.agents_home_dirs().is_empty());
    assert!(
        !defaults
            .agent_filenames()
            .iter()
            .any(|name| name.starts_with(".claude/"))
    );

    assert_eq!(
        COMPAT_CELLS
            .into_iter()
            .filter(|cell| cell.is_runtime_supported())
            .map(|cell| {
                (
                    Into::<&'static str>::into(cell.vendor()),
                    Into::<&'static str>::into(cell.surface()),
                )
            })
            .collect::<Vec<_>>(),
        [
            ("cursor", "skills"),
            ("cursor", "rules"),
            ("cursor", "agents"),
            ("cursor", "mcps"),
            ("cursor", "hooks"),
            ("cursor", "sessions"),
            ("claude", "skills"),
            ("claude", "rules"),
            ("claude", "agents"),
            ("claude", "mcps"),
            ("claude", "hooks"),
            ("claude", "sessions"),
            ("claude", "settings"),
            ("claude", "plugins"),
            ("codex", "sessions"),
        ]
    );
}

#[test]
fn skill_config_dirs_all_on_lists_every_vendor() {
    assert_eq!(
        vec![".cortex", ".agents", ".claude", ".cursor"],
        CompatConfig::all_enabled().skill_config_dirs()
    );
}

#[test]
fn skill_config_dirs_gates_each_vendor() {
    let mut c = CompatConfig::all_enabled();
    c.cursor.skills = false;
    assert_eq!(vec![".cortex", ".agents", ".claude"], c.skill_config_dirs());

    c.claude.skills = false;
    assert_eq!(vec![".cortex", ".agents"], c.skill_config_dirs());

    let mut c2 = CompatConfig::all_enabled();
    c2.claude.skills = false;
    assert_eq!(
        vec![".cortex", ".agents", ".cursor"],
        c2.skill_config_dirs()
    );
}

#[test]
fn rules_dirs_all_on_lists_every_vendor() {
    assert_eq!(
        vec![".cortex/rules", ".claude/rules", ".cursor/rules"],
        CompatConfig::all_enabled().rules_dirs()
    );
}

#[test]
fn rules_dirs_gates_each_vendor() {
    let mut c = CompatConfig::all_enabled();
    c.cursor.rules = false;
    assert_eq!(vec![".cortex/rules", ".claude/rules"], c.rules_dirs());
    c.claude.rules = false;
    assert_eq!(vec![".cortex/rules"], c.rules_dirs());
}

#[test]
fn agent_filenames_all_on_include_the_claude_subdir() {
    assert_eq!(
        vec![
            "Agents.md",
            "Claude.md",
            "CLAUDE.md",
            "CLAUDE.local.md",
            "AGENT.md",
            "AGENTS.md",
            ".claude/CLAUDE.md",
            ".claude/CLAUDE.local.md",
        ],
        CompatConfig::all_enabled().agent_filenames()
    );
}

#[test]
fn agent_filenames_drops_claude_subdir_when_off() {
    let mut c = CompatConfig::all_enabled();
    c.claude.agents = false;
    assert_eq!(
        vec![
            "Agents.md",
            "Claude.md",
            "CLAUDE.md",
            "CLAUDE.local.md",
            "AGENT.md",
            "AGENTS.md",
        ],
        c.agent_filenames()
    );
}

#[test]
fn agents_home_dirs_all_on_lists_every_vendor() {
    assert_eq!(
        vec![".claude", ".cursor"],
        CompatConfig::all_enabled().agents_home_dirs()
    );
}

#[test]
fn agents_home_dirs_gates_each_vendor() {
    let mut c = CompatConfig::all_enabled();
    c.claude.agents = false;
    assert_eq!(vec![".cursor"], c.agents_home_dirs());
    c.cursor.agents = false;
    assert!(c.agents_home_dirs().is_empty());
}

#[test]
fn toml_struct_deserializes_partial_cells() {
    let parsed: CompatConfigToml =
        toml::from_str("[cursor]\nskills = false\nsessions = true\n[codex]\nsessions = true\n")
            .unwrap();
    assert_eq!(Some(false), parsed.cursor.skills);
    assert_eq!(None, parsed.cursor.rules);
    assert_eq!(Some(true), parsed.cursor.sessions);
    assert_eq!(VendorCompatToml::default(), parsed.claude);
    assert_eq!(Some(true), parsed.codex.sessions);
    assert_eq!(None, parsed.codex.skills);

    let parsed: CompatConfigToml = toml::from_str("[claude]\nmcps = false\n").unwrap();
    assert_eq!(Some(false), parsed.claude.mcps);
    assert_eq!(None, parsed.claude.hooks);
    assert_eq!(None, parsed.claude.sessions);
    assert_eq!(VendorCompatToml::default(), parsed.cursor);
    assert_eq!(VendorCompatToml::default(), parsed.codex);
}

#[test]
fn claude_settings_and_plugins_are_opt_in_and_fail_closed() {
    type Resolver = fn(Option<&toml::Value>, &CompatEnv, Option<&RemoteSettings>) -> bool;
    for (surface, env_var, resolve) in [
        (
            "settings",
            "CORTEX_CLAUDE_SETTINGS_ENABLED",
            resolve_compat_claude_settings as Resolver,
        ),
        (
            "plugins",
            "CORTEX_CLAUDE_PLUGINS_ENABLED",
            resolve_compat_claude_plugins as Resolver,
        ),
    ] {
        let none = CompatEnv::default();
        let empty = toml::Value::Table(toml::map::Map::new());
        let on: toml::Value =
            toml::from_str(&format!("[compat.claude]\n{surface} = true")).unwrap();
        let malformed: toml::Value =
            toml::from_str(&format!("[compat.claude]\n{surface} = \"yes\"")).unwrap();

        assert!(!resolve(Some(&empty), &none, None), "{surface} defaults on");
        assert!(!resolve(None, &none, None), "{surface} on without a config");
        assert!(resolve(Some(&on), &none, None), "{surface} opt-in ignored");
        assert!(
            !resolve(Some(&malformed), &none, None),
            "{surface} malformed turned on"
        );
        assert!(
            !resolve(Some(&on), &env_with(&[(env_var, false)]), None),
            "{env_var}=false must outrank config"
        );
        assert!(
            resolve(Some(&empty), &env_with(&[(env_var, true)]), None),
            "{env_var}=true must turn {surface} on"
        );
    }
}
