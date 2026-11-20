use super::*;
use crate::config::{
    agent,
    test_support::{env_lock, EnvGuard},
    ToolServerConfig,
};
use harnx_core::hooks::HookEventKind;
use std::fs;
use std::path::Path;

fn write(root: &Path, path: &str, text: &str) -> std::path::PathBuf {
    let file = root.join(path);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, text).unwrap();
    file
}

fn agent_md(front_matter: &str) -> String {
    format!("---\n{}\n---\nPrompt", front_matter)
}

fn stop_entry(name: &str, source: &str) -> String {
    format!("{{name: {name}, bash: '{source}'}}")
}

fn start_entry(name: &str, source: &str) -> String {
    format!("{{name: {name}, bash: '{source}'}}")
}

fn event_block(event: &str, entries: &[String]) -> String {
    format!(
        "  {event}:\n{}",
        entries
            .iter()
            .map(|entry| format!("    - {entry}"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

fn hooks_yaml(event: &str, entries: &[String]) -> String {
    format!("hooks:\n{}\n", event_block(event, entries))
}

fn tool_server_yaml(hooks: &str) -> String {
    format!("command: unused\n{hooks}")
}

fn load_error(file: &Path) -> String {
    format!("{:#}", Config::load_from_file(file).unwrap_err())
}

#[test]
fn event_hooks_three_levels_replace_in_place_and_disable_without_source() {
    let _lock = env_lock();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", root);
    let file = write(
        root,
        "config.yaml",
        &format!(
            "hooks:\n{}\n{}\n",
            event_block(
                "Stop",
                &[
                    stop_entry("audit", "global"),
                    stop_entry("stable", "global")
                ]
            ),
            event_block("SessionStart", &[start_entry("greet", "global")])
        ),
    );
    // Write in reverse order to check the loader, not filesystem enumeration order.
    write(
        root,
        "tool_servers/z.yaml",
        &tool_server_yaml(&hooks_yaml("Stop", &[stop_entry("z", "next")])),
    );
    write(
        root,
        "tool_servers/a.yaml",
        &format!(
            "{}\n{}",
            "command: unused",
            hooks_yaml(
                "Stop",
                &[stop_entry("audit", "server"), stop_entry("a", "next")]
            )
        ),
    );
    let agent_file = write(
        root,
        "agents/test.md",
        &agent_md(&hooks_yaml(
            "Stop",
            &[
                stop_entry("audit", "agent"),
                "{name: stable, enabled: false}".to_string(),
                stop_entry("agent_last", "next"),
            ],
        )),
    );
    let mut config = Config::load_from_file(&file).unwrap();
    config
        .use_agent_obj(agent::load(&agent_file).unwrap())
        .unwrap();
    let resolved = config.resolved_event_hooks().unwrap();
    assert_eq!(
        resolved
            .stop
            .iter()
            .map(|hook| hook.name.as_str())
            .collect::<Vec<_>>(),
        ["audit", "a", "z", "agent_last"]
    );
    assert_eq!(
        resolved.stop[0].source,
        MiddlewareSource::Bash("agent".into())
    );
    // A per-event override doesn't leak into other events.
    assert_eq!(resolved.session_start.len(), 1);
    assert_eq!(resolved.session_start[0].name, "greet");
}

#[test]
fn event_hooks_disable_requires_an_earlier_entry() {
    let _lock = env_lock();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", root);
    let file = write(root, "config.yaml", "{}\n");
    let agent = write(
        root,
        "agents/test.md",
        &agent_md(&hooks_yaml(
            "Stop",
            &["{name: ghost, enabled: false}".to_string()],
        )),
    );
    let mut config = Config::load_from_file(&file).unwrap();
    let error = config
        .use_agent_obj(agent::load(&agent).unwrap())
        .unwrap_err();
    assert!(error.to_string().contains("ghost"), "{error:#}");
}

#[test]
fn event_hooks_invalid_entries_fail_load_naming_the_hook() {
    let _lock = env_lock();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", root);
    for fields in [
        "bash: next, unknown: true",
        "bash: next, python: pass",
        "bash: next, script: missing.sh",
        "script: missing.py",
        "script: unknown.js",
        "enabled: false",
        "timeout: 0s",
        "on_error: deny",
        "fetch: ['*']",
    ] {
        let file = write(
            root,
            "config.yaml",
            &hooks_yaml("Stop", &[format!("{{name: broken, {fields}}}")]),
        );
        let error = load_error(&file);
        assert!(error.contains("broken"), "{fields}: {error}");
        fs::remove_file(&file).unwrap();
    }
    // on_error: deny is valid for UserPromptSubmit and fails config load for Stop.
    let file = write(
        root,
        "config.yaml",
        &hooks_yaml(
            "UserPromptSubmit",
            &["{name: gate, bash: block 'no', on_error: deny}".to_string()],
        ),
    );
    Config::load_from_file(&file).unwrap();
}

#[test]
fn event_hooks_legacy_command_entries_fail_load_with_guidance() {
    let _lock = env_lock();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", root);
    let file = write(
        root,
        "config.yaml",
        "hooks:\n  entries:\n    - command: /x.sh\n",
    );
    let error = load_error(&file);
    assert!(error.contains("legacy"), "{error}");
    assert!(error.contains("middleware"), "{error}");

    let file = write(
        root,
        "config.yaml",
        "hooks:\n  Stop:\n    - command: /x.sh\n",
    );
    let error = load_error(&file);
    assert!(error.contains("legacy `command:`"), "{error}");

    let file = write(root, "config.yaml", "hooks:\n  PostToolUse: []\n");
    let error = load_error(&file);
    assert!(error.contains("PostToolUse"), "{error}");
}

#[test]
fn event_hooks_scripts_use_declaring_file_directory() {
    let _lock = env_lock();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", root);
    write(
        root,
        "elsewhere/on-session-start.sh",
        "add_context \"right\"",
    );
    write(
        root,
        "on-session-start.sh",
        "add_context \"wrong directory\"",
    );
    let file = write(
        root,
        "elsewhere/config.yaml",
        &hooks_yaml(
            "SessionStart",
            &["{name: greet, script: on-session-start.sh}".to_string()],
        ),
    );
    let config = Config::load_from_file(&file).unwrap();
    let resolved = config.resolved_event_hooks().unwrap();
    assert_eq!(
        resolved.session_start[0].source,
        MiddlewareSource::Bash("add_context \"right\"".into())
    );
}

#[test]
fn event_hooks_tool_server_and_agent_scripts_bind_their_origins() {
    let _lock = env_lock();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", root);
    let file = write(root, "config.yaml", "{}\n");
    write(
        root,
        "tool_servers/scripts/greet.sh",
        "add_context \"server\"",
    );
    write(
        root,
        "tool_servers/user.yaml",
        &tool_server_yaml(&hooks_yaml(
            "SessionStart",
            &["{name: greet, script: scripts/greet.sh}".to_string()],
        )),
    );
    write(root, "packages/pkg/scripts/stop.py", "resume(\"continue\")");
    let agent = write(
        root,
        "packages/pkg/agents/package.md",
        &agent_md(&hooks_yaml(
            "Stop",
            &["{name: keep-going, script: scripts/stop.py}".to_string()],
        )),
    );
    let mut config = Config::load_from_file(&file).unwrap();
    config
        .use_agent_obj(agent::load_with_qualified_name(&agent, "pkg/package").unwrap())
        .unwrap();
    let resolved = config.resolved_event_hooks().unwrap();
    assert_eq!(
        resolved.session_start[0].source,
        MiddlewareSource::Bash("add_context \"server\"".into())
    );
    assert_eq!(
        resolved.stop[0].source,
        MiddlewareSource::Python("resume(\"continue\")".into())
    );
}

#[test]
fn event_hooks_same_name_across_tool_servers_replaces_in_place() {
    let _lock = env_lock();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", root);
    let file = write(root, "config.yaml", "{}\n");
    write(
        root,
        "tool_servers/a.yaml",
        &tool_server_yaml(&hooks_yaml("Stop", &[stop_entry("duplicate", "first")])),
    );
    write(
        root,
        "packages/pkg/tool_servers/b.yaml",
        &tool_server_yaml(&hooks_yaml("Stop", &[stop_entry("duplicate", "second")])),
    );
    let config = Config::load_from_file(&file).unwrap();
    let resolved = config.resolved_event_hooks().unwrap();
    // The chain stays name-unique: the later server's entry replaces the
    // earlier one in place.
    assert_eq!(resolved.stop.len(), 1);
    assert_eq!(resolved.stop[0].name, "duplicate");
    assert_eq!(
        resolved.stop[0].source,
        MiddlewareSource::Bash("second".into())
    );
}

#[test]
fn event_hooks_python_source_and_fetch_settings_resolve() {
    let _lock = env_lock();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", root);
    let file = write(
        root,
        "config.yaml",
        &format!(
            "{}\n",
            hooks_yaml(
                "UserPromptSubmit",
                &[
                    "{name: policy, python: 'pass', timeout: 45s, fetch: ['example.com']}"
                        .to_string()
                ]
            )
        ),
    );
    let config = Config::load_from_file(&file).unwrap();
    let resolved = config.resolved_event_hooks().unwrap();
    assert_eq!(
        resolved.user_prompt_submit[0].timeout,
        std::time::Duration::from_secs(45)
    );
    assert_eq!(resolved.user_prompt_submit[0].fetch, vec!["example.com"]);
    assert!(matches!(
        resolved.user_prompt_submit[0].source,
        MiddlewareSource::Python(_)
    ));
}

#[test]
fn event_hooks_resolution_survives_default_config() {
    let _lock = env_lock();
    let temp = tempfile::tempdir().unwrap();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", temp.path());
    let config = Config::default();
    let resolved = config.resolved_event_hooks().unwrap();
    assert!(resolved.is_empty());
    assert!(resolved.list(HookEventKind::Stop).is_empty());
    let _: Option<Box<HooksConfig>> = config.hooks.clone();
    let _: Option<ToolServerConfig> = None;
}

#[test]
fn event_hooks_patched_scripts_keep_loader_origins_for_every_event() {
    let _lock = env_lock();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", root);
    let config_file = write(root, "config.yaml", "{}\n");
    for (name, script_dir) in [("tailored", "external"), ("pkg/tailored", "packages/pkg")] {
        write(
            root,
            &format!("{script_dir}/scripts/baseline.sh"),
            "event_input",
        );
        write(
            root,
            &format!("{script_dir}/scripts/patched.py"),
            "event_input()",
        );
        let mut hooks = serde_json::json!({"max_resume": 2});
        for event in HookEventKind::ALL {
            hooks[event.key()] = serde_json::json!([
                {"name": event.key(), "script": "scripts/baseline.sh"}
            ]);
        }
        let frontmatter = serde_json::json!({
            "hooks": hooks,
            "patches": ".hooks |= with_entries(if .key == \"max_resume\" then . else .value |= map(.script = \"scripts/patched.py\") end)"
        });
        // The local agent lives outside agents/, so fallback origins cannot
        // accidentally make a lost loader origin pass this test.
        let agent_file = write(
            root,
            "external/tailored.md",
            &agent_md(&serde_yaml::to_string(&frontmatter).unwrap()),
        );
        let agent = agent::load_with_qualified_name(&agent_file, name).unwrap();
        let origin = agent.hooks().unwrap().origin.clone().unwrap();
        let mut config = Config::load_from_file(&config_file).unwrap();
        config.use_agent_obj(agent).unwrap();

        config
            .apply_session_agent_patches(
                &harnx_core::user_principals::anonymous(),
                &harnx_core::agent_config::AgentVariables::default(),
            )
            .unwrap();

        let agent = config.extract_agent();
        let hooks = agent.hooks().unwrap();
        assert_eq!(hooks.origin.as_ref(), Some(&origin), "{name}");
        assert_eq!(hooks.max_resume, Some(2));
        let resolved = config.resolved_event_hooks().unwrap();
        for event in HookEventKind::ALL {
            let entries = resolved.list(event);
            assert_eq!(entries.len(), 1, "{name}: {}", event.key());
            assert_eq!(entries[0].name, event.key());
            assert_eq!(
                entries[0].source,
                MiddlewareSource::Python("event_input()".into())
            );
        }
    }
}
