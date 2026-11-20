//! Worker-level tests for an agent's `patches` applied at session-agent
//! installation: the shared effective-agent builder in
//! `daemon::install_session_metadata_agent` must make every `Config.agent`
//! reader see the patched agent, fail closed on a broken filter, and derive
//! fresh from the baseline file on every activation.

use super::daemon::install_session_metadata_agent;
use super::server_reconciler::tool_servers_for_activation;
use crate::config::{Config, ConfigLock, ToolServerConfig};
use crate::nats_session_metadata::{
    user_principals, SessionInitializer, SessionMetadata, SessionOverrides,
};
use crate::test_environment::EnvGuard;
use harnx_core::require_nextest;
use harnx_core::user_principals::{from_resolved, UserPrincipals};
use std::sync::Arc;

fn developer_principals() -> UserPrincipals {
    from_resolved(
        Some("alice"),
        None,
        &["developers".into(), "engineering".into()],
        &[],
    )
}

fn error_chain(error: anyhow::Error) -> String {
    format!("{error:#}")
}

fn outsider_principals() -> UserPrincipals {
    from_resolved(Some("mallory"), None, &["finance".into()], &[])
}

fn make_tool_decl(name: &str) -> harnx_core::tool::ToolDeclaration {
    harnx_core::tool::ToolDeclaration {
        name: name.to_string(),
        description: format!("tool {name}"),
        parameters: Default::default(),
        mcp_tool_name: None,
        mcp_server_name: None,
        call_template: None,
        result_template: None,
        idempotent_hint: None,
        read_only_hint: None,
        kind: None,
    }
}

/// A fixture config directory with an OpenAI test client that resolves both
/// fixture models, plus one agent file whose contents the caller supplies.
// qual:allow(srp, god_struct) reason: "one test fixture owns its temp dir, config, and derived metadata/globals; splitting them would restate the fixture setup in every test"
struct AgentFixture {
    _temp: tempfile::TempDir,
    config: Config,
}

impl AgentFixture {
    fn open(agent_markdown: &str) -> Self {
        let temp = tempfile::TempDir::new().unwrap();
        let config_dir = temp.path();
        std::fs::create_dir_all(config_dir.join("clients")).unwrap();
        std::fs::create_dir_all(config_dir.join("agents")).unwrap();
        std::fs::write(
            config_dir.join("clients").join("openai.yaml"),
            "type: openai\napi_key: sk-test\nmodels:\n  - name: test-model\n    type: chat\n    max_input_tokens: 4096\n  - name: test-model-mini\n    type: chat\n    max_input_tokens: 4096\n",
        )
        .unwrap();
        std::fs::write(config_dir.join("config.yaml"), "model: openai:test-model\n").unwrap();
        std::fs::write(
            config_dir.join("agents").join("tailored.md"),
            agent_markdown,
        )
        .unwrap();
        // Config::agent_file resolves through HARNX_CONFIG_DIR, so the guard
        // must stay alive for the whole test.
        std::env::set_var("HARNX_CONFIG_DIR", config_dir);
        let config = Config::load_from_file(&config_dir.join("config.yaml")).unwrap();
        Self {
            _temp: temp,
            config,
        }
    }

    fn global(&self) -> Arc<ConfigLock> {
        Arc::new(ConfigLock::new(self.config.clone()))
    }

    fn metadata(&self, principals: &UserPrincipals) -> SessionMetadata {
        self.metadata_with(principals, SessionOverrides::default())
    }

    fn metadata_with(
        &self,
        principals: &UserPrincipals,
        overrides: SessionOverrides,
    ) -> SessionMetadata {
        let mut initializer = SessionInitializer::named("tailored", Default::default())
            .with_user_id(principals.user_id.as_deref().unwrap_or("anonymous"))
            .with_user_principals(principals);
        initializer.overrides = overrides;
        SessionMetadata::new("patch-session", initializer)
    }
}

fn patch_tailoring_markdown(patch_body: &str) -> String {
    format!(
        "---\n\
         model: openai:test-model\n\
         use_tools: [fs_read, jira_search]\n\
         bash_tools: [fs_*]\n\
         variables:\n\
         \x20 - name: team\n\
         \x20   description: Owning team\n\
         \x20   default: platform\n\
         hooks:\n\
         \x20 Stop:\n\
         \x20   - name: audit\n\
         \x20     bash: event_input\n\
         middleware:\n\
         \x20 - name: audit\n\
         \x20   match: '^bash_exec$'\n\
         \x20   bash: echo audited\n\
         \x20   tools: []\n\
         run_limits:\n\
         \x20 timeout_secs: 100\n\
         patches:\n\
         \x20 - '{patch_body}'\n\
         ---\n\
         Tailored agent.\n"
    )
}

fn install(fixture: &AgentFixture, metadata: &SessionMetadata) -> anyhow::Result<()> {
    let global = fixture.global();
    install_session_metadata_agent(&global, metadata)
}

/// Trims fs_read from `use_tools`, fs_* from `bash_tools`, swaps in a patched
/// hook entry, and rewrites the run-limit override — everything a
/// non-developer session of the same agent must see.
const OUTSIDER_PATCH: &str = "if ($user.groups | index(\"developers\")) then . else \
.use_tools |= map(select(. != \"fs_read\")) \
| .bash_tools = [\"jira_*\"] \
| .hooks = {\"Stop\": [{\"name\": \"patched-audit\", \"bash\": \"event_input\"}]} \
| .run_limits = {\"timeout_secs\": 3600} end";

#[test]
fn install_applies_patches_and_every_reader_sees_the_effective_agent() {
    require_nextest();
    let fixture = AgentFixture::open(&patch_tailoring_markdown(OUTSIDER_PATCH));
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", fixture._temp.path());

    // Developer session: nothing trimmed.
    let developer_global = fixture.global();
    developer_global
        .write()
        .nats_tool_declarations
        .write()
        .extend([make_tool_decl("fs_read"), make_tool_decl("jira_search")]);
    install_session_metadata_agent(
        &developer_global,
        &fixture.metadata(&developer_principals()),
    )
    .unwrap();
    let developer_agent = developer_global.read().extract_agent();
    assert_eq!(
        developer_agent.use_tools(),
        Some(vec!["fs_read".to_string(), "jira_search".to_string()])
    );
    assert_eq!(developer_agent.bash_tools(), Some(vec!["fs_*".to_string()]));
    assert_eq!(
        developer_agent
            .user_principals()
            .map(|user| user.groups.clone()),
        Some(vec!["developers".to_string(), "engineering".to_string()]),
        "the snapshot must be carried on the effective agent"
    );

    // Outsider session of the same agent: everything trimmed, fresh from the
    // baseline file (no accumulation with the developer install).
    let outsider_global = fixture.global();
    outsider_global
        .write()
        .nats_tool_declarations
        .write()
        .extend([make_tool_decl("fs_read"), make_tool_decl("jira_search")]);
    install_session_metadata_agent(&outsider_global, &fixture.metadata(&outsider_principals()))
        .unwrap();

    let outsider_agent = outsider_global.read().extract_agent();
    // Model tool declarations: the selection the model would see.
    assert_eq!(
        outsider_agent.use_tools(),
        Some(vec!["jira_search".to_string()])
    );
    // Bash execution catalog, not just the declaration list.
    let bash_catalog: Vec<String> = outsider_global
        .read()
        .bash_inner_tool_declarations()
        .into_iter()
        .map(|declaration| declaration.name)
        .collect();
    assert_eq!(
        bash_catalog,
        vec!["jira_search".to_string()],
        "a trimmed bash_tools selector must remove the tool from the Bash catalog"
    );
    let developer_catalog: Vec<String> = developer_global
        .read()
        .bash_inner_tool_declarations()
        .into_iter()
        .map(|declaration| declaration.name)
        .collect();
    assert_eq!(developer_catalog, vec!["fs_read".to_string()]);
    // Hooks: the patched entry reaches the reader the agent loop uses.
    let hooks = outsider_global.read().resolved_event_hooks().unwrap();
    assert_eq!(
        hooks
            .stop
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        vec!["patched-audit"]
    );
    // Run limits: what freeze_activation_limits reads from Config.agent.
    assert_eq!(
        outsider_agent
            .run_limits()
            .and_then(|limits| limits.timeout_secs)
            .and_then(|timeout| timeout.as_finite_secs()),
        Some(3600)
    );
    // Middleware: the effective agent's entry reaches the chain wiring.
    let middleware_names: Vec<String> = outsider_global
        .read()
        .resolved_middleware()
        .unwrap()
        .entries
        .iter()
        .map(|entry| entry.name.clone())
        .collect();
    assert!(middleware_names.contains(&"audit".to_string()));
    // Agent variables survive the patch round trip.
    assert_eq!(
        outsider_agent
            .shared_variables()
            .get("team")
            .map(String::as_str),
        Some("platform"),
        "agent variables must survive the effective-agent build"
    );
    assert_eq!(
        outsider_agent
            .user_principals()
            .map(|user| user.groups.clone()),
        Some(vec!["finance".to_string()])
    );
    // The baseline file on disk is unchanged.
    let on_disk =
        std::fs::read_to_string(fixture._temp.path().join("agents").join("tailored.md")).unwrap();
    assert_eq!(on_disk, patch_tailoring_markdown(OUTSIDER_PATCH));
}

#[test]
fn session_overrides_run_before_patches_and_cannot_re_add_trimmed_tools() {
    require_nextest();
    let fixture = AgentFixture::open(&patch_tailoring_markdown(
        ".use_tools |= map(select(. != \"fs_read\"))",
    ));
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", fixture._temp.path());

    // The override re-adds exactly what the patch removes; the patch runs
    // last, so the trimmed selector must win.
    let overrides = SessionOverrides {
        use_tools: Some(vec!["fs_read".to_string(), "jira_search".to_string()]),
        temperature: Some(0.7),
        ..Default::default()
    };
    let metadata = fixture.metadata_with(&outsider_principals(), overrides);
    let global = fixture.global();
    install_session_metadata_agent(&global, &metadata).unwrap();
    assert_eq!(
        global.read().extract_agent().use_tools(),
        Some(vec!["jira_search".to_string()]),
        "an override must not re-add what the patch removed"
    );
    assert_eq!(
        global.read().extract_agent().temperature(),
        Some(0.7),
        "a non-conflicting session override must survive alongside the patch"
    );
}

#[test]
fn a_patch_changing_the_model_resolves_the_new_model() {
    require_nextest();
    let fixture = AgentFixture::open(&patch_tailoring_markdown(
        "if ($user.groups | index(\"developers\")) then . else .model = \"openai:test-model-mini\" end",
    ));
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", fixture._temp.path());

    let global = fixture.global();
    install_session_metadata_agent(&global, &fixture.metadata(&outsider_principals())).unwrap();
    let agent = global.read().extract_agent();
    assert_eq!(agent.model_id(), Some("openai:test-model-mini"));
    assert_eq!(
        agent.model().id(),
        "openai:test-model-mini",
        "the patched model id must be resolved to a runtime Model"
    );
    assert_eq!(
        agent.shared_variables().get("team").map(String::as_str),
        Some("platform"),
        "agent variables must survive the model change"
    );
    // The developer path keeps the original resolved model.
    let developer_global = fixture.global();
    install_session_metadata_agent(
        &developer_global,
        &fixture.metadata(&developer_principals()),
    )
    .unwrap();
    assert_eq!(
        developer_global.read().extract_agent().model().id(),
        "openai:test-model"
    );
}

#[test]
fn patches_clearing_the_model_use_the_global_model() {
    require_nextest();
    for filter in [".model = null", "del(.model)"] {
        let mut fixture = AgentFixture::open(&format!(
            "---\nmodel: openai:test-model-mini\npatches: '{filter}'\n---\nBody\n"
        ));
        let _env = EnvGuard::new("HARNX_CONFIG_DIR", fixture._temp.path());
        fixture.config.set_model("openai:test-model").unwrap();
        let baseline = fixture.config.retrieve_agent("tailored").unwrap();
        assert_eq!(baseline.model().id(), "openai:test-model-mini");

        let global = fixture.global();
        install_session_metadata_agent(&global, &fixture.metadata(&outsider_principals())).unwrap();

        let config = global.read();
        assert_eq!(config.model.id(), "openai:test-model");
        assert_eq!(config.extract_agent().model().id(), "openai:test-model");
        assert_eq!(
            config.current_model_id().as_deref(),
            Some("openai:test-model")
        );
    }
}

#[test]
fn patched_model_and_middleware_errors_leave_the_active_agent_unchanged() {
    require_nextest();
    for (filter, expected_error) in [
        (
            r#".use_tools = [] | .model = "missing-client:missing-model""#,
            "Unknown chat model 'missing-client:missing-model'",
        ),
        (
            r#".model = "openai:test-model-mini" | .middleware[0].match = "[""#,
            "invalid match regex",
        ),
    ] {
        let fixture = AgentFixture::open(&patch_tailoring_markdown(filter));
        let _env = EnvGuard::new("HARNX_CONFIG_DIR", fixture._temp.path());
        let mut config = fixture.config.clone();
        config.use_agent_by_name("tailored").unwrap();
        config
            .agent
            .as_mut()
            .unwrap()
            .set_user_principals(Some(developer_principals()));
        let before = format!("{:?}", config.agent);

        let error = config
            .apply_session_agent_patches(
                &outsider_principals(),
                &harnx_core::agent_config::AgentVariables::default(),
            )
            .unwrap_err();
        let message = error_chain(error);

        assert!(
            message.contains("tailored") && message.contains("patches"),
            "{message}"
        );
        assert!(message.contains(expected_error), "{message}");
        assert_eq!(format!("{:?}", config.agent), before, "{message}");
        assert!(
            config.resolved_middleware().is_ok(),
            "the original middleware must still resolve"
        );
    }
}

#[test]
fn a_broken_patch_fails_the_install_naming_the_agent() {
    require_nextest();
    // Compiles at load, fails at activation: the filter indexes a field on
    // every string in use_tools.
    let fixture = AgentFixture::open(&patch_tailoring_markdown(".use_tools |= map(.missing)"));
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", fixture._temp.path());

    let error = install(&fixture, &fixture.metadata(&outsider_principals())).unwrap_err();
    let message = error_chain(error);
    assert!(
        message.contains("tailored") && message.contains("patches"),
        "activation error must name the agent and `patches`: {message}"
    );
}

#[test]
fn a_legacy_session_without_a_snapshot_still_installs() {
    require_nextest();
    let fixture = AgentFixture::open(&patch_tailoring_markdown(
        ".use_tools |= map(select(. != \"fs_read\"))",
    ));
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", fixture._temp.path());

    // No user_principals property, no user_id: the legacy fallback is
    // anonymous, so the patch applies with empty groups.
    let metadata = SessionMetadata::new(
        "legacy-session",
        SessionInitializer::named("tailored", Default::default()),
    );
    let global = fixture.global();
    install_session_metadata_agent(&global, &metadata).unwrap();
    assert_eq!(
        user_principals(&metadata),
        harnx_core::user_principals::anonymous()
    );
    assert_eq!(
        global.read().extract_agent().use_tools(),
        Some(vec!["jira_search".to_string()]),
        "an anonymous snapshot must see the trimmed config"
    );
    assert_eq!(
        global
            .read()
            .extract_agent()
            .user_principals()
            .map(|user| user.is_anonymous()),
        Some(true)
    );
}

fn tool_server(name: &str) -> ToolServerConfig {
    ToolServerConfig {
        middleware: Default::default(),
        name: name.to_string(),
        command: format!("harnx-{name}-server"),
        args: Vec::new(),
        env: Default::default(),
        enabled: true,
        description: None,
        package: None,
        hooks: None,
    }
}

/// The scratch install behind server selection sees the same effective agent,
/// so a selector removed by the patch creates no server demand.
#[test]
fn tool_servers_for_activation_demands_servers_for_the_patched_agent() {
    require_nextest();
    let fixture = AgentFixture::open(&patch_tailoring_markdown(
        "if ($user.groups | index(\"developers\")) then . else .use_tools |= map(select(. != \"fs_read\")) | .bash_tools = [] end",
    ));
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", fixture._temp.path());

    let mut config = fixture.config.clone();
    config.tool_servers = vec![tool_server("fs"), tool_server("jira")];
    let global = Arc::new(ConfigLock::new(config));

    let developer_demand: Vec<String> =
        tool_servers_for_activation(&global, &fixture.metadata(&developer_principals()))
            .into_iter()
            .map(|server| server.name)
            .collect();
    assert_eq!(developer_demand, vec!["fs".to_string(), "jira".to_string()]);

    let outsider_demand: Vec<String> =
        tool_servers_for_activation(&global, &fixture.metadata(&outsider_principals()))
            .into_iter()
            .map(|server| server.name)
            .collect();
    assert_eq!(
        outsider_demand,
        vec!["jira".to_string()],
        "the fs selector was patched out, so the fs server needs no demand"
    );

    // A broken patch fails the scratch install too; no server is started for
    // a config that was never proven to run.
    let broken = AgentFixture::open(&patch_tailoring_markdown(".use_tools |= map(.missing)"));
    let _broken_env = EnvGuard::new("HARNX_CONFIG_DIR", broken._temp.path());
    let mut broken_config = broken.config.clone();
    broken_config.tool_servers = vec![tool_server("fs"), tool_server("jira")];
    let broken_global = Arc::new(ConfigLock::new(broken_config));
    assert!(
        tool_servers_for_activation(&broken_global, &broken.metadata(&outsider_principals()))
            .is_empty(),
        "a broken patch must create no server demand"
    );
}

#[test]
fn rebuilt_and_regenerated_inputs_keep_patched_config_and_principals() {
    use crate::config::{input, session};

    require_nextest();
    let fixture = AgentFixture::open(&patch_tailoring_markdown(
        r#".model = "openai:test-model-mini" | .use_tools = ["jira_search"] | .bash_tools = ["jira_*"] | .instructions = "Patched for {{ __user__.user_id }}: {{ __user__.groups[0] }} ({{ team }})""#,
    ));
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", fixture._temp.path());
    let global = fixture.global();
    let baseline = global.read().retrieve_agent("tailored").unwrap();
    let mut stale = input::from_str(&global, "original prompt", Some(baseline));
    assert!(stale.agent().user_principals().is_none());
    assert_eq!(stale.agent().model().id(), "openai:test-model");

    install_session_metadata_agent(&global, &fixture.metadata(&outsider_principals())).unwrap();
    let active_session = session::new(&global.read(), "input-resume", None).unwrap();
    global.write().session = Some(active_session);

    // Stop-hook and pending-context continuations both rebuild through this path.
    let rebuilt = input::from_str(&global, "resume with pending context", None);
    assert!(rebuilt.with_session());
    assert_patched_input(&rebuilt);
    let messages = input::build_messages(&rebuilt, &global).unwrap();
    assert_eq!(
        messages[0].content.to_text(),
        "Patched for mallory: finance (platform)"
    );

    // Refresh a genuinely stale same-name agent, not an already-patched clone.
    input::set_regenerate(&mut stale, &global);
    assert!(stale.regenerate());
    assert_patched_input(&stale);
}

fn assert_patched_input(input: &harnx_core::input::Input) {
    let agent = input.agent();
    assert_eq!(agent.use_tools(), Some(vec!["jira_search".to_string()]));
    assert_eq!(agent.bash_tools(), Some(vec!["jira_*".to_string()]));
    assert_eq!(agent.model().id(), "openai:test-model-mini");
    assert_eq!(agent.user_principals(), Some(&outsider_principals()));
    assert_eq!(
        agent.system_text_with_tools(&[]).unwrap(),
        "Patched for mallory: finance (platform)"
    );
}

fn variable_patch_fixture(filter: &str, prompt: &str) -> AgentFixture {
    let frontmatter = serde_json::json!({
        "model": "openai:test-model",
        "variables": [{"name": "color", "description": "Color", "default": "blue"}],
        "patches": filter
    });
    AgentFixture::open(&format!(
        "---\n{}---\n{prompt}\n",
        serde_yaml::to_string(&frontmatter).unwrap()
    ))
}

#[test]
fn patched_variable_defaults_render_with_original_explicit_values() {
    require_nextest();
    for explicit in [false, true] {
        let fixture = variable_patch_fixture(
            r#".variables[0].default = "red" | .variables += [{name: "added", description: "Added", default: "new"}]"#,
            "{{ color }}/{{ added }}",
        );
        let _env = EnvGuard::new("HARNX_CONFIG_DIR", fixture._temp.path());
        let mut metadata = fixture.metadata(&outsider_principals());
        if explicit {
            metadata.variables.insert("color".into(), "green".into());
            metadata
                .variables
                .insert("added".into(), "explicit-new".into());
        }
        let global = fixture.global();

        install_session_metadata_agent(&global, &metadata).unwrap();

        let agent = global.read().extract_agent();
        assert_eq!(
            agent.interpolated_instructions().unwrap().trim(),
            if explicit {
                "green/explicit-new"
            } else {
                "red/new"
            }
        );
    }
}

#[test]
fn patched_file_variable_defaults_use_the_worker_resolution_path() {
    require_nextest();
    let fixture = variable_patch_fixture(
        r#".variables[0].path = "color.txt" | .variables += [{name: "added", description: "Added", path: "added.txt"}]"#,
        "{{ color }}/{{ added }}",
    );
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", fixture._temp.path());
    std::fs::write(fixture._temp.path().join("agents/color.txt"), "red").unwrap();
    std::fs::write(fixture._temp.path().join("agents/added.txt"), "from-file").unwrap();
    let global = fixture.global();

    install_session_metadata_agent(&global, &fixture.metadata(&outsider_principals())).unwrap();

    assert_eq!(
        global
            .read()
            .extract_agent()
            .interpolated_instructions()
            .unwrap()
            .trim(),
        "red/from-file"
    );
}

#[test]
fn patches_preserve_resolved_variable_maps_when_declarations_do_not_change() {
    require_nextest();
    let fixture = variable_patch_fixture(".instructions = \"{{ color }}\"", "Body");
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", fixture._temp.path());
    let mut config = fixture.config.clone();
    config.use_agent_by_name("tailored").unwrap();
    let agent = config.agent.as_mut().unwrap();
    agent.set_shared_variables(harnx_core::agent_config::AgentVariables::from_iter([
        ("color".into(), "resolved-value".into()),
        ("runtime-only".into(), "keep".into()),
    ]));
    let before = agent.shared_variables().clone();

    config
        .apply_session_agent_patches(
            &outsider_principals(),
            &harnx_core::agent_config::AgentVariables::default(),
        )
        .unwrap();

    let agent = config.extract_agent();
    assert_eq!(agent.shared_variables(), &before);
    assert_eq!(agent.interpolated_instructions().unwrap(), "resolved-value");
}

#[test]
fn patched_required_or_missing_file_variables_fail_without_publishing() {
    require_nextest();
    for (filter, expected_error) in [
        (
            r#".variables += [{name: "required", description: "Required"}]"#,
            "required",
        ),
        (r#".variables[0].path = "missing.txt""#, "missing.txt"),
    ] {
        let fixture = variable_patch_fixture(filter, "{{ color }}");
        let _env = EnvGuard::new("HARNX_CONFIG_DIR", fixture._temp.path());
        let mut config = fixture.config.clone();
        config.use_agent_by_name("tailored").unwrap();
        let before = format!("{:?}", config.agent);

        let error = config
            .apply_session_agent_patches(
                &outsider_principals(),
                &harnx_core::agent_config::AgentVariables::default(),
            )
            .unwrap_err();

        let message = error_chain(error);
        assert!(message.contains(expected_error), "{message}");
        assert!(
            message.contains("tailored") && message.contains("patches"),
            "{message}"
        );
        assert_eq!(format!("{:?}", config.agent), before);
    }
}

#[test]
fn install_patch_can_supply_a_required_variable_default() {
    require_nextest();
    for explicit in [false, true] {
        let fixture = AgentFixture::open(
            r#"---
model: openai:test-model
variables:
  - name: color
    description: Required color
patches:
  - '.variables[0].default = "red"'
---
{{ color }}
"#,
        );
        let _env = EnvGuard::new("HARNX_CONFIG_DIR", fixture._temp.path());
        let mut metadata = fixture.metadata(&outsider_principals());
        if explicit {
            metadata.variables.insert("color".into(), "green".into());
        }
        let global = fixture.global();

        install_session_metadata_agent(&global, &metadata).unwrap();

        assert_eq!(
            global
                .read()
                .extract_agent()
                .interpolated_instructions()
                .unwrap()
                .trim(),
            if explicit { "green" } else { "red" }
        );
    }
}

#[test]
fn install_missing_effective_variable_fails_with_its_name() {
    require_nextest();
    for (default, patches) in [
        (None, vec![]),
        (None, vec!["."]),
        (Some("blue"), vec!["del(.variables[0].default)"]),
    ] {
        let frontmatter = serde_json::json!({
            "model": "openai:test-model",
            "variables": [{"name": "color", "description": "Required color", "default": default}],
            "patches": patches
        });
        let fixture = AgentFixture::open(&format!(
            "---\n{}---\n{{{{ color }}}}\n",
            serde_yaml::to_string(&frontmatter).unwrap()
        ));
        let _env = EnvGuard::new("HARNX_CONFIG_DIR", fixture._temp.path());
        let metadata = fixture.metadata(&outsider_principals());

        let error = install_session_metadata_agent(&fixture.global(), &metadata).unwrap_err();

        let message = error_chain(error);
        assert!(message.contains("color"), "{message}");
        assert!(message.contains("tailored"), "{message}");
    }
}
