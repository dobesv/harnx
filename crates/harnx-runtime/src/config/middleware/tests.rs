use super::*;
use crate::config::{
    agent,
    servers_split::apply_tool_server_patch,
    test_support::{env_lock, EnvGuard},
    ToolServerConfig,
};
use std::fs;

fn write(root: &Path, path: &str, text: &str) -> std::path::PathBuf {
    let file = root.join(path);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, text).unwrap();
    file
}

fn full(name: &str, source: &str) -> String {
    format!("{{name: {name}, match: '.*', bash: '{source}'}}")
}

fn tool_server_yaml(entry: &str) -> String {
    format!("command: unused\nmiddleware: [{entry}]\n")
}

fn load_error(file: &Path) -> String {
    format!("{:#}", Config::load_from_file(file).unwrap_err())
}

#[test]
fn middleware_three_levels_replace_in_place_and_disable_without_source() {
    let _lock = env_lock();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", root);
    let file = write(
        root,
        "config.yaml",
        &format!(
            "middleware: [{}, {}, {}]\n",
            full("audit", "global"),
            full("gate", "next"),
            full("stable", "next")
        ),
    );
    // Write in reverse order to check the loader, not filesystem enumeration order.
    write(
        root,
        "tool_servers/z.yaml",
        &tool_server_yaml(&full("z", "next")),
    );
    write(
        root,
        "tool_servers/a.yaml",
        &format!(
            "command: unused\nmiddleware: [{}, {}]\n",
            full("audit", "server"),
            full("a", "next")
        ),
    );
    write(
        root,
        "packages/zzz/tool_servers/a.yaml",
        &tool_server_yaml(&full("package_z", "next")),
    );
    write(
        root,
        "packages/aaa/tool_servers/z.yaml",
        &tool_server_yaml(&full("package_a", "next")),
    );
    let agent_file = write(
        root,
        "agents/test.md",
        &format!(
            "---\nmiddleware: [{}, {{name: gate, enabled: false}}, {}]\n---\nPrompt",
            full("audit", "agent"),
            full("agent_last", "next")
        ),
    );
    let mut config = Config::load_from_file(&file).unwrap();
    config
        .use_agent_obj(agent::load(&agent_file).unwrap())
        .unwrap();
    let chain = config.resolved_middleware().unwrap();
    assert_eq!(
        chain
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        [
            "audit",
            "stable",
            "a",
            "z",
            "package_a",
            "package_z",
            "agent_last"
        ]
    );
    assert_eq!(
        chain.entries[0].source,
        MiddlewareSource::Bash("agent".into())
    );
    assert_eq!(chain.entries[0].origin.file, agent_file);
    assert_eq!(chain.entries[0].timeout, std::time::Duration::from_secs(30));
}

#[test]
fn middleware_config_load_errors_identify_the_entry() {
    let _lock = env_lock();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", root);
    for fields in [
        "match: '.*', bash: next, unknown: true",
        "match: '.*', bash: next, python: call_next()",
        "bash: next",
        "match: '[', bash: next",
        "match: '.*', script: missing.sh",
        "match: '.*', script: unknown.js",
        "enabled: false",
    ] {
        let file = write(
            root,
            "config.yaml",
            &format!("middleware: [{{name: broken, {fields}}}]\n"),
        );
        let error = load_error(&file);
        assert!(error.contains("broken"), "{fields}: {error}");
    }
}

#[test]
fn middleware_invalid_tool_server_and_package_entries_fail_load() {
    let _lock = env_lock();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", root);
    let file = write(root, "config.yaml", "{}");
    for directory in ["tool_servers", "packages/pkg/tool_servers"] {
        for fields in [
            "match: '[', bash: next",
            "match: '.*', script: missing.py",
            "enabled: false",
            "match: '.*', bash: next, unknown: true",
        ] {
            let server = write(
                root,
                &format!("{directory}/bad.yaml"),
                &format!("command: unused\nmiddleware: [{{name: broken, {fields}}}]"),
            );
            let error = load_error(&file);
            assert!(error.contains("broken"), "{directory}: {error}");
            fs::remove_file(server).unwrap();
        }
    }
}

#[test]
fn middleware_duplicate_tool_server_names_in_one_file_fail_load() {
    let _lock = env_lock();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", root);
    let file = write(root, "config.yaml", "{}");
    write(
        root,
        "tool_servers/bad.yaml",
        &format!(
            "command: unused\nmiddleware: [{}, {}]",
            full("duplicate", "next"),
            full("duplicate", "next")
        ),
    );
    let error = load_error(&file);
    assert!(error.contains("duplicate name"), "{error}");
    assert!(error.contains("duplicate"), "{error}");
}

#[test]
fn middleware_global_scripts_use_declaring_file_not_config_dir_or_cwd() {
    let _lock = env_lock();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", root);
    write(root, "elsewhere/check.py", "call_next()");
    write(root, "check.py", "wrong directory");
    let file = write(
        root,
        "elsewhere/config.yaml",
        "middleware: [{name: check, match: '.*', script: check.py}]",
    );
    let config = Config::load_from_file(&file).unwrap();
    let chain = config.resolved_middleware().unwrap();
    assert_eq!(
        chain.entries[0].source,
        MiddlewareSource::Python("call_next()".into())
    );
    assert_eq!(chain.entries[0].origin.base_dir, root.join("elsewhere"));
}

#[test]
fn middleware_scripts_use_tool_server_file_directory_and_package_root() {
    let _lock = env_lock();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", root);
    let file = write(root, "config.yaml", "{}");
    write(root, "tool_servers/scripts/user.bash", "next # user");
    write(
        root,
        "tool_servers/user.yaml",
        "command: unused\nmiddleware: [{name: user, match: '.*', script: scripts/user.bash}]",
    );
    write(root, "packages/pkg/scripts/package.sh", "next # package");
    write(
        root,
        "packages/pkg/tool_servers/package.yaml",
        "command: unused\nmiddleware: [{name: package, match: '.*', script: scripts/package.sh}]",
    );
    let config = Config::load_from_file(&file).unwrap();
    let chain = config.resolved_middleware().unwrap();
    assert_eq!(
        chain.entries[0].source,
        MiddlewareSource::Bash("next # user".into())
    );
    assert_eq!(chain.entries[0].origin.base_dir, root.join("tool_servers"));
    assert_eq!(
        chain.entries[1].source,
        MiddlewareSource::Bash("next # package".into())
    );
    assert_eq!(chain.entries[1].origin.base_dir, root.join("packages/pkg"));
}

#[test]
fn middleware_agent_scripts_use_file_directory_or_package_root() {
    let _lock = env_lock();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", root);
    let file = write(root, "config.yaml", "{}");
    write(root, "agents/scripts/user.sh", "next # user");
    let user = write(
        root,
        "agents/user.md",
        "---\nmiddleware: [{name: check, match: '.*', script: scripts/user.sh}]\n---\nPrompt",
    );
    write(root, "packages/pkg/scripts/package.py", "call_next()");
    let package = write(
        root,
        "packages/pkg/agents/package.md",
        "---\nmiddleware: [{name: check, match: '.*', script: scripts/package.py}]\n---\nPrompt",
    );
    let mut config = Config::load_from_file(&file).unwrap();
    config.use_agent_obj(agent::load(&user).unwrap()).unwrap();
    assert_eq!(
        config.resolved_middleware().unwrap().entries[0].source,
        MiddlewareSource::Bash("next # user".into())
    );
    config
        .use_agent_obj(agent::load_with_qualified_name(&package, "pkg/package").unwrap())
        .unwrap();
    let chain = config.resolved_middleware().unwrap();
    assert_eq!(
        chain.entries[0].source,
        MiddlewareSource::Python("call_next()".into())
    );
    assert_eq!(chain.entries[0].origin.base_dir, root.join("packages/pkg"));
}

#[test]
fn middleware_invalid_agent_fails_before_becoming_active() {
    let _lock = env_lock();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", root);
    let file = write(root, "config.yaml", "{}");
    let mut config = Config::load_from_file(&file).unwrap();
    let missing = write(
        root,
        "agents/missing.md",
        "---\nmiddleware: [{name: missing_script, match: '.*', script: missing.sh}]\n---\nPrompt",
    );
    assert!(format!("{:#}", agent::load(&missing).unwrap_err()).contains("missing_script"));
    let unknown = write(
        root,
        "agents/unknown.md",
        "---\nmiddleware: [{name: unknown_gate, enabled: false}]\n---\nPrompt",
    );
    let error = config
        .use_agent_obj(agent::load(&unknown).unwrap())
        .unwrap_err();
    assert!(error.to_string().contains("unknown_gate"));
    assert!(config.agent.is_none());
    let error = config.retrieve_agent("unknown").unwrap_err();
    assert!(error.to_string().contains("unknown_gate"));
}

#[test]
fn middleware_disable_requires_an_entry_still_present_from_an_earlier_level() {
    let _lock = env_lock();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", root);
    let file = write(
        root,
        "config.yaml",
        &format!("middleware: [{}]", full("gate", "next")),
    );
    write(
        root,
        "tool_servers/a.yaml",
        "command: unused\nmiddleware: [{name: gate, enabled: false}]",
    );
    let mut config = Config::load_from_file(&file).unwrap();
    assert!(config.resolved_middleware().unwrap().entries.is_empty());
    let agent = AgentConfig::from_markdown(
        "test",
        "---\nmiddleware: [{name: gate, enabled: false}]\n---\nPrompt",
    )
    .unwrap();
    assert!(config
        .use_agent_obj(crate::config::Agent::new(agent))
        .unwrap_err()
        .to_string()
        .contains("gate"));
}

#[test]
fn middleware_version_tracks_script_text_not_just_path() {
    let _lock = env_lock();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", root);
    write(root, "check.sh", "next");
    let file = write(
        root,
        "config.yaml",
        "middleware: [{name: check, match: '.*', script: check.sh}]",
    );
    let config = Config::load_from_file(&file).unwrap();
    let first = config.resolved_middleware().unwrap().version;
    write(root, "check.sh", "next # edited");
    assert_ne!(first, config.resolved_middleware().unwrap().version);
}

#[test]
fn middleware_tool_server_patch_keeps_script_origin() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write(root, "scripts/check.sh", "next");
    write(
        root,
        "tool_servers/check.yaml",
        "command: unused\nmiddleware: [{name: check, match: '.*', script: scripts/check.sh}]",
    );
    let mut servers =
        Config::load_tool_servers_with_middleware_base(&root.join("tool_servers"), Some(root))
            .unwrap();
    let origin = servers[0].middleware.origin.clone();
    apply_tool_server_patch(&mut servers[0], &[".description = \"patched\"".into()]).unwrap();
    assert_eq!(servers[0].middleware.origin, origin);
    apply_tool_server_patch(
        &mut servers[0],
        &[".middleware[0].timeout = \"45s\"".into()],
    )
    .unwrap();
    assert_eq!(servers[0].middleware.origin, origin);
}

#[test]
fn middleware_tool_server_rejects_legacy_hooks_entries() {
    let legacy = "command: unused\nhooks:\n  entries:\n    - command: legacy-hook\n      legacy_extra: accepted\n";
    let error = serde_yaml::from_str::<ToolServerConfig>(legacy).unwrap_err();
    assert!(format!("{error}").contains("legacy"), "{error}");
    let combined = serde_yaml::from_str::<ToolServerConfig>(&format!(
        "{legacy}middleware: [{}]",
        full("audit", "next")
    ))
    .unwrap_err()
    .to_string();
    assert!(combined.contains("legacy"), "{combined}");
}

#[test]
fn middleware_same_name_across_tool_servers_replaces_in_place() {
    let _lock = env_lock();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", root);
    let file = write(root, "config.yaml", "{}");
    write(
        root,
        "tool_servers/a.yaml",
        &format!(
            "command: unused\nmiddleware: [{}, {}]",
            full("first", "next"),
            full("shared", "from-user-server")
        ),
    );
    write(
        root,
        "packages/pkg/tool_servers/b.yaml",
        &format!(
            "command: unused\nmiddleware: [{}]",
            full("shared", "from-package-server")
        ),
    );
    let config = Config::load_from_file(&file).unwrap();
    let chain = config.resolved_middleware().unwrap();
    // Replacement keeps the earlier position; the chain stays name-unique.
    assert_eq!(
        chain
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        ["first", "shared"]
    );
    assert_eq!(
        chain.entries[1].source,
        MiddlewareSource::Bash("from-package-server".into())
    );
    assert_eq!(
        chain.entries[1].origin.file,
        root.join("packages/pkg/tool_servers/b.yaml")
    );
}

#[test]
fn middleware_disable_in_later_tool_server_removes_earlier_server_entry() {
    let _lock = env_lock();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", root);
    let file = write(root, "config.yaml", "{}");
    write(
        root,
        "tool_servers/a.yaml",
        &format!(
            "command: unused\nmiddleware: [{}]",
            full("shared", "from-user-server")
        ),
    );
    write(
        root,
        "packages/pkg/tool_servers/b.yaml",
        "command: unused\nmiddleware: [{name: shared, enabled: false}]",
    );
    let config = Config::load_from_file(&file).unwrap();
    assert!(config.resolved_middleware().unwrap().entries.is_empty());
}

#[test]
fn middleware_two_packages_shipping_same_server_name_resolve_one_chain_entry() {
    // Mirrors the shipped pantheon/coding bash.yaml: both packages declare the
    // same tool server with middleware of the same name.
    let _lock = env_lock();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", root);
    let file = write(root, "config.yaml", "{}");
    let entry = "{name: proxy-auth, match: '^bash_(exec|spawn)$', bash: next}";
    for package in ["aaa", "zzz"] {
        write(
            root,
            &format!("packages/{package}/tool_servers/bash.yaml"),
            &format!("command: unused\nmiddleware: [{entry}]"),
        );
    }
    let config = Config::load_from_file(&file).unwrap();
    let chain = config.resolved_middleware().unwrap();
    assert_eq!(chain.entries.len(), 1);
    assert_eq!(chain.entries[0].name, "proxy-auth");
    assert_eq!(
        chain.entries[0].origin.file,
        root.join("packages/zzz/tool_servers/bash.yaml")
    );
}

#[test]
fn middleware_script_must_be_a_readable_utf8_file() {
    let _lock = env_lock();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", root);
    let file = write(
        root,
        "config.yaml",
        "middleware: [{name: invalid_script, match: '.*', script: check.py}]",
    );
    fs::create_dir(root.join("check.py")).unwrap();
    let error = Config::load_from_file(&file).unwrap_err().to_string();
    assert!(error.contains("invalid_script"), "{error}");
    fs::remove_dir(root.join("check.py")).unwrap();
    fs::write(root.join("check.py"), [0xff]).unwrap();
    let error = Config::load_from_file(&file).unwrap_err().to_string();
    assert!(error.contains("invalid_script"), "{error}");
}

#[test]
fn middleware_relative_origin_is_frozen_at_load_time() {
    let relative = file_origin(Path::new("relative/config.yaml"), None).unwrap();
    assert!(relative.file.is_absolute());
    assert!(relative.base_dir.is_absolute());
    assert_eq!(
        relative.base_dir.join("check.py"),
        std::env::current_dir().unwrap().join("relative/check.py")
    );
}
