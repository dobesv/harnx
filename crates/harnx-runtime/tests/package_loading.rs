use std::{ffi::OsString, path::Path, sync::Mutex};

use harnx_core::package_namespace::resolve_package_relative_name;
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
fn package_loading_test_package_client_patch_preserves_qualified_name() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", tmp.path());

    install_test_package(
        tmp.path(),
        "mypkg",
        &[
            (
                "clients/openai.yaml",
                "type: openai
api_key: sk-original
",
            ),
            (
                "agents/worker.md",
                "---
model: openai:gpt-4o
---
You work.",
            ),
        ],
    );

    // The package patch file is a SIBLING of the package directory:
    // packages/<pkg>.patch.yaml (see harnx_core::config_paths::package_patch_file).
    std::fs::write(
        tmp.path().join("packages").join("mypkg.patch.yaml"),
        "clients:
  - 'if .name == \"openai\" then .api_key = \"patched-key\" else . end'
",
    )
    .unwrap();

    let config = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(Config::init(harnx_runtime::config::WorkingMode::Cmd, false))
        .expect("config should load");

    let client = config
        .clients
        .iter()
        .find(|client| client.effective_name() == "mypkg/openai")
        .expect("expected qualified package client named mypkg/openai");

    assert_eq!(client.effective_name(), "mypkg/openai");
    match client {
        harnx_client::ClientConfig::OpenAIConfig(c) => {
            assert_eq!(c.api_key.as_deref(), Some("patched-key"));
            assert_eq!(c.package.as_deref(), Some("mypkg"));
        }
        _ => panic!("expected OpenAIConfig variant, got: {client:?}"),
    }
}

#[test]
fn package_loading_test_package_client_loaded_with_qualified_name() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", tmp.path());

    install_test_package(
        tmp.path(),
        "mypkg",
        &[(
            "clients/openai.yaml",
            "type: openai
",
        )],
    );

    let config = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(Config::init(harnx_runtime::config::WorkingMode::Cmd, false))
        .expect("config should load");

    assert!(
        config
            .clients
            .iter()
            .any(|client| client.effective_name() == "mypkg/openai"),
        "Expected qualified package client in config.clients, got: {:?}",
        config
            .clients
            .iter()
            .map(|client| client.effective_name().to_string())
            .collect::<Vec<_>>()
    );
}

#[test]
fn package_loading_test_package_agent_model_rewritten_to_qualified() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", tmp.path());

    install_test_package(
        tmp.path(),
        "mypkg",
        &[
            (
                "clients/openai.yaml",
                "type: openai
",
            ),
            (
                "agents/worker.md",
                "---
model: openai:gpt-4o
---
You work.",
            ),
        ],
    );

    let config = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(Config::init(harnx_runtime::config::WorkingMode::Cmd, false))
        .expect("config should load");
    let agent = config.retrieve_agent("mypkg/worker").unwrap();

    assert_eq!(agent.model_id(), Some("mypkg/openai:gpt-4o"));
}

#[test]
fn package_loading_test_package_agent_model_leading_slash_rewritten_to_top_level() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", tmp.path());

    std::fs::create_dir_all(tmp.path().join("clients")).unwrap();
    std::fs::write(
        tmp.path().join("clients/openai.yaml"),
        "type: openai
",
    )
    .unwrap();
    install_test_package(
        tmp.path(),
        "mypkg",
        &[(
            "agents/worker.md",
            "---
model: /openai:gpt-4o
---
You work.",
        )],
    );

    let config = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(Config::init(harnx_runtime::config::WorkingMode::Cmd, false))
        .expect("config should load");
    let agent = config.retrieve_agent("mypkg/worker").unwrap();

    assert_eq!(agent.model_id(), Some("openai:gpt-4o"));
}

#[test]
fn package_loading_test_package_agent_model_cross_package_unchanged() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", tmp.path());

    install_test_package(
        tmp.path(),
        "other",
        &[(
            "clients/openai.yaml",
            "type: openai
",
        )],
    );
    install_test_package(
        tmp.path(),
        "mypkg",
        &[(
            "agents/worker.md",
            "---
model: other/openai:gpt-4o
---
You work.",
        )],
    );

    let config = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(Config::init(harnx_runtime::config::WorkingMode::Cmd, false))
        .expect("config should load");
    let agent = config.retrieve_agent("mypkg/worker").unwrap();

    assert_eq!(agent.model_id(), Some("other/openai:gpt-4o"));
}

#[test]
fn package_loading_test_package_clients_same_bare_name_do_not_collide() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", tmp.path());

    install_test_package(
        tmp.path(),
        "pkg-a",
        &[(
            "clients/openai.yaml",
            "type: openai
",
        )],
    );
    install_test_package(
        tmp.path(),
        "pkg-b",
        &[(
            "clients/openai.yaml",
            "type: openai
",
        )],
    );

    let config = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(Config::init(harnx_runtime::config::WorkingMode::Cmd, false))
        .expect("config should load");

    let client_names = config
        .clients
        .iter()
        .map(|client| client.effective_name().to_string())
        .collect::<Vec<_>>();
    assert!(client_names.contains(&"pkg-a/openai".to_string()));
    assert!(client_names.contains(&"pkg-b/openai".to_string()));
}

#[test]
fn package_loading_test_top_level_agent_model_stays_unchanged() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", tmp.path());

    std::fs::create_dir_all(tmp.path().join("clients")).unwrap();
    std::fs::write(
        tmp.path().join("clients/openai.yaml"),
        "type: openai
",
    )
    .unwrap();
    std::fs::create_dir_all(tmp.path().join("agents")).unwrap();
    std::fs::write(
        tmp.path().join("agents/worker.md"),
        "---
model: openai:gpt-4o
---
You work.",
    )
    .unwrap();

    let config = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(Config::init(harnx_runtime::config::WorkingMode::Cmd, false))
        .expect("config should load");
    let agent = config.retrieve_agent("worker").unwrap();

    assert_eq!(agent.model_id(), Some("openai:gpt-4o"));
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
fn package_loading_test_compaction_agent_bare_name_resolves_within_package() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", tmp.path());

    install_test_package(
        tmp.path(),
        "mypkg",
        &[
            (
                "agents/main.md",
                "---\ncompaction_agent: compactor\n---\nYou are the main agent.",
            ),
            (
                "agents/compactor.md",
                "---\nrole: compaction\n---\nSummarize for mypkg.",
            ),
        ],
    );

    let agents_dir = tmp.path().join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(
        agents_dir.join("compactor.md"),
        "---\nrole: compaction\n---\nTop-level summarizer.",
    )
    .unwrap();

    let resolved = resolve_package_relative_name("compactor", Some("mypkg"));
    assert_eq!(resolved, "mypkg/compactor");

    let config = Config::default();
    let package_compactor = config.retrieve_agent(&resolved).unwrap();
    assert_eq!(package_compactor.name(), "mypkg/compactor");
    let package_prompt = package_compactor.interpolated_instructions().unwrap();
    assert!(
        package_prompt.contains("mypkg"),
        "Expected package-scoped compactor prompt, got: {:?}",
        package_prompt
    );

    let top_level_compactor = config.retrieve_agent("compactor").unwrap();
    assert_eq!(top_level_compactor.name(), "compactor");
    let top_level_prompt = top_level_compactor.interpolated_instructions().unwrap();
    assert!(
        top_level_prompt.contains("Top-level"),
        "Expected top-level compactor prompt, got: {:?}",
        top_level_prompt
    );
}

#[test]
fn package_loading_test_compaction_agent_leading_slash_resolves_top_level() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", tmp.path());

    install_test_package(
        tmp.path(),
        "mypkg",
        &[(
            "agents/main.md",
            "---\ncompaction_agent: /compactor\n---\nYou are the main agent.",
        )],
    );

    let agents_dir = tmp.path().join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(
        agents_dir.join("compactor.md"),
        "---\nrole: compaction\n---\nTop-level summarizer.",
    )
    .unwrap();

    let resolved = resolve_package_relative_name("/compactor", Some("mypkg"));
    assert_eq!(resolved, "compactor");

    let config = Config::default();
    let top_level_compactor = config.retrieve_agent(&resolved).unwrap();
    assert_eq!(top_level_compactor.name(), "compactor");
    let top_level_prompt = top_level_compactor.interpolated_instructions().unwrap();
    assert!(
        top_level_prompt.contains("Top-level"),
        "Expected top-level compactor prompt, got: {:?}",
        top_level_prompt
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
        enabled: true,
        description: None,
        rename_tools: Default::default(),
        tool_templates: Default::default(),
        package: Some("mypkg".to_string()),
        hooks: None,
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
/// Test that in-file `name:` is ignored and client name comes from filename.
/// This directly verifies issue #823: client name should come from YAML filename stem,
/// NOT from a `name:` field in file contents.
#[test]
fn package_loading_test_client_name_ignored_from_file_contents() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", tmp.path());

    // Create clients directory at root level
    std::fs::create_dir_all(tmp.path().join("clients")).unwrap();

    // Write a client file named "foo.yaml" but with `name: bar` inside
    // The effective_name should be "foo" (from filename), NOT "bar" (from file contents)
    std::fs::write(
        tmp.path().join("clients/foo.yaml"),
        "type: claude\nname: bar\n",
    )
    .unwrap();

    let config = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(Config::init(harnx_runtime::config::WorkingMode::Cmd, false))
        .expect("config should load");

    // Find the client and verify its effective_name is "foo" (filename stem), not "bar"
    let client = config
        .clients
        .iter()
        .find(|c| c.effective_name() == "foo")
        .expect("expected client named 'foo' from filename, not 'bar' from file contents");

    // Also verify no client named "bar" exists (proving name: field was ignored)
    assert!(
        !config.clients.iter().any(|c| c.effective_name() == "bar"),
        "client 'bar' should not exist - in-file name: field must be ignored"
    );

    // Verify the client is a ClaudeConfig variant (correct deserialization)
    match client {
        harnx_client::ClientConfig::ClaudeConfig(c) => {
            assert_eq!(c.name, "foo");
        }
        _ => panic!("expected ClaudeConfig variant, got: {:?}", client),
    }
}
