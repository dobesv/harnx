use std::{ffi::OsString, path::Path, sync::Mutex};

use harnx_runtime::config::{complete_agent_variables, list_agents, list_assistant_agents, Config};

static ENV_MUTEX: Mutex<()> = Mutex::new(());

struct EnvGuard {
    key: &'static str,
    prev: Option<OsString>,
}

impl EnvGuard {
    fn new(key: &'static str, value: impl AsRef<Path>) -> Self {
        let prev = std::env::var_os(key);
        unsafe { std::env::set_var(key, value.as_ref()) };
        Self { key, prev }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.prev {
            Some(value) => unsafe { std::env::set_var(self.key, value) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

fn install_test_package(config_dir: &Path, pkg_name: &str, files: &[(&str, &str)]) {
    let pkg_dir = config_dir.join("packages").join(pkg_name);
    std::fs::create_dir_all(&pkg_dir).unwrap();

    for (rel_path, content) in files {
        let full_path = pkg_dir.join(rel_path);
        std::fs::create_dir_all(full_path.parent().unwrap()).unwrap();
        std::fs::write(full_path, content).unwrap();
    }

    let manifest = format!(
        "name: {pkg_name}\nsource:\n  type: git\n  url: file:///fake\n  tag: v1.0.0\n  commit: abc123\ninstalled_at: \"2025-01-01T00:00:00Z\"\n"
    );
    std::fs::write(pkg_dir.join("manifest.yaml"), manifest).unwrap();
}

#[test]
fn package_loading_test_package_agent_appears_in_list_agents() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", tmp.path());

    install_test_package(
        tmp.path(),
        "mypkg",
        &[("agents/coder.md", "---\nmodel: test\n---\nYou code.")],
    );

    let agents = list_agents();
    assert!(
        agents.contains(&"mypkg/coder".to_string()),
        "Expected 'mypkg/coder' in agents, got: {agents:?}"
    );
}

#[test]
fn package_loading_test_package_agent_not_listed_bare() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", tmp.path());

    install_test_package(
        tmp.path(),
        "mypkg",
        &[("agents/coder.md", "---\nmodel: test\n---\nYou code.")],
    );

    let agents = list_agents();
    assert!(
        !agents.contains(&"coder".to_string()),
        "Bare 'coder' should not be in agents list: {agents:?}"
    );
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn package_loading_test_package_assistant_agent_appears_in_list_assistant_agents() {
    // Regression test for issue #569: agent picker did not show agents from packages
    // because list_assistant_agents() only scanned the top-level agents/ directory.
    // ENV_MUTEX serializes tests that mutate HARNX_CONFIG_DIR; holding the std::sync
    // guard across the await is acceptable here because no other task contends for it.
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", tmp.path());

    // An agent with default (Assistant) role
    install_test_package(
        tmp.path(),
        "mypkg",
        &[("agents/helper.md", "---\nmodel: test\n---\nI help.")],
    );

    let agents = list_assistant_agents().await;
    assert!(
        agents.contains(&"mypkg/helper".to_string()),
        "Expected 'mypkg/helper' in list_assistant_agents, got: {agents:?}"
    );
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn package_loading_test_package_subagent_not_in_list_assistant_agents() {
    // Agents with role: subagent should NOT appear in the picker.
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", tmp.path());

    install_test_package(
        tmp.path(),
        "mypkg",
        &[(
            "agents/worker.md",
            "---\nmodel: test\nrole: subagent\n---\nI work silently.",
        )],
    );

    let agents = list_assistant_agents().await;
    assert!(
        !agents.contains(&"mypkg/worker".to_string()),
        "Subagent 'mypkg/worker' should not appear in list_assistant_agents: {agents:?}"
    );
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn package_loading_test_multiple_packages_assistant_sorted_deduped() {
    // list_assistant_agents() across multiple packages must be sorted and deduplicated.
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", tmp.path());

    install_test_package(
        tmp.path(),
        "zeta",
        &[("agents/assist.md", "---\nmodel: test\n---\nZeta assistant.")],
    );
    install_test_package(
        tmp.path(),
        "alpha",
        &[(
            "agents/assist.md",
            "---\nmodel: test\n---\nAlpha assistant.",
        )],
    );

    let agents = list_assistant_agents().await;
    assert!(
        agents.contains(&"alpha/assist".to_string()),
        "Expected 'alpha/assist' in list_assistant_agents, got: {agents:?}"
    );
    assert!(
        agents.contains(&"zeta/assist".to_string()),
        "Expected 'zeta/assist' in list_assistant_agents, got: {agents:?}"
    );
    // Sorted: alpha before zeta
    let alpha_pos = agents.iter().position(|s| s == "alpha/assist").unwrap();
    let zeta_pos = agents.iter().position(|s| s == "zeta/assist").unwrap();
    assert!(
        alpha_pos < zeta_pos,
        "Expected sorted order (alpha before zeta), got: {agents:?}"
    );
    // No duplicates
    let deduped: Vec<_> = {
        let mut v = agents.clone();
        v.dedup();
        v
    };
    assert_eq!(
        agents, deduped,
        "list_assistant_agents() must not contain duplicates"
    );
}

#[test]
fn package_loading_test_package_agent_variable_completion() {
    // Regression test: complete_agent_variables() must resolve package agents via
    // packages/<pkg>/agents/<stem>.md, not agents/<pkg>/<stem>.md.
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", tmp.path());

    install_test_package(
        tmp.path(),
        "mypkg",
        &[(
            "agents/helper.md",
            "---\nmodel: test\nvariables:\n  - name: TARGET\n    description: The target to help with\n---\nI help with $TARGET.",
        )],
    );

    let vars = complete_agent_variables("mypkg/helper");
    assert!(
        !vars.is_empty(),
        "Expected variable completions for 'mypkg/helper', got none"
    );
    let names: Vec<&str> = vars.iter().map(|(k, _)| k.as_str()).collect();
    assert!(
        names.contains(&"TARGET="),
        "Expected 'TARGET=' in completions, got: {names:?}"
    );
}

#[test]
fn package_loading_test_multiple_packages_no_collision() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", tmp.path());

    install_test_package(
        tmp.path(),
        "pkg-a",
        &[(
            "agents/helper.md",
            "---\nmodel: test\n---\nPackage A helper.",
        )],
    );
    install_test_package(
        tmp.path(),
        "pkg-b",
        &[(
            "agents/helper.md",
            "---\nmodel: test\n---\nPackage B helper.",
        )],
    );

    let agents = list_agents();
    assert!(
        agents.contains(&"pkg-a/helper".to_string()),
        "Expected 'pkg-a/helper' in agents, got: {agents:?}"
    );
    assert!(
        agents.contains(&"pkg-b/helper".to_string()),
        "Expected 'pkg-b/helper' in agents, got: {agents:?}"
    );
}

#[test]
fn package_loading_test_package_agent_file_resolves_correctly() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", tmp.path());

    install_test_package(
        tmp.path(),
        "mypkg",
        &[("agents/coder.md", "---\nmodel: test\n---\nYou code.")],
    );

    let path = Config::agent_file("mypkg/coder");
    assert_eq!(
        path,
        tmp.path()
            .join("packages")
            .join("mypkg")
            .join("agents")
            .join("coder.md")
    );
    assert!(
        path.exists(),
        "Expected resolved agent path to exist: {}",
        path.display()
    );
}

#[test]
fn package_loading_test_mcp_server_display_names_for_agent() {
    // Verify that mcp_server_display_name logic (exercised via reinit_managers_for_agent)
    // produces bare names for same-package servers and prefixed names for others.
    //
    // We test this at the unit level via the free functions exposed through Config,
    // since McpManager startup requires real MCP server processes.
    // The naming logic itself is the invariant; process startup is tested elsewhere.

    use harnx_mcp::McpServerConfig;

    // Construct a same-package server (bare_name = "fs", package = "mypkg")
    let same_pkg_server = McpServerConfig {
        name: "fs".to_string(),
        command: "echo".to_string(),
        args: vec![],
        env: Default::default(),
        roots: vec![],
        enabled: true,
        description: None,
        rename_tools: Default::default(),
        tool_templates: Default::default(),
        package: Some("mypkg".to_string()),
    };

    // Construct a different-package server
    let other_pkg_server = McpServerConfig {
        name: "db".to_string(),
        package: Some("otherpkg".to_string()),
        ..same_pkg_server.clone()
    };

    // Top-level server (no package)
    let top_level_server = McpServerConfig {
        name: "bash".to_string(),
        package: None,
        ..same_pkg_server.clone()
    };

    // When active agent is "mypkg/coder":
    let agent_pkg = Some("mypkg");
    // Same package → bare name
    assert_eq!(
        harnx_runtime::mcp_server_display_name_for_test(&same_pkg_server, agent_pkg),
        "fs"
    );
    // Other package → prefixed
    assert_eq!(
        harnx_runtime::mcp_server_display_name_for_test(&other_pkg_server, agent_pkg),
        "otherpkg__db"
    );
    // Top-level → unchanged
    assert_eq!(
        harnx_runtime::mcp_server_display_name_for_test(&top_level_server, agent_pkg),
        "bash"
    );

    // When no active agent (None):
    // Same package → prefixed (global view)
    assert_eq!(
        harnx_runtime::mcp_server_display_name_for_test(&same_pkg_server, None),
        "mypkg__fs"
    );
}
