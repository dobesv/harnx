//! Tests for the agent module (extracted from agent.rs for code health).
#![cfg(test)]

use super::*;
use crate::client::MessageRole;
use crate::config::test_support::EnvGuard;
use crate::config::GlobalConfig;
use crate::utils::create_abort_signal;
use harnx_core::config_paths::config_dir;
use std::{
    fs,
    path::Path,
    path::PathBuf,
    sync::{LazyLock, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

static TEST_CONFIG_DIR_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn write_agent_dump_test_config() {
    let config_dir = config_dir();
    fs::create_dir_all(config_dir.join("clients")).unwrap();
    fs::write(
        config_dir.join("config.yaml"),
        "show_timestamps: false\nshow_sequence_numbers: false\n",
    )
    .unwrap();
    fs::write(
        config_dir.join("clients").join("openai.yaml"),
        "type: openai\napi_key: sk-test\nmodels:\n  - name: gpt-4o\n    type: chat\n    max_input_tokens: 4096\n",
    )
    .unwrap();
}

fn unique_test_config_dir() -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "harnx-agent-test-{}-{timestamp}",
        std::process::id()
    ))
}

fn with_test_config_dir<T>(f: impl FnOnce(&Path) -> Result<T>) -> Result<T> {
    let _guard = TEST_CONFIG_DIR_LOCK.lock().unwrap();
    let config_dir = unique_test_config_dir();
    let data_dir = config_dir.with_file_name(format!(
        "{}-data",
        config_dir.file_name().unwrap().to_string_lossy()
    ));
    let state_dir = config_dir.with_file_name(format!(
        "{}-state",
        config_dir.file_name().unwrap().to_string_lossy()
    ));
    let agents_dir = config_dir.join("agents");
    fs::create_dir_all(&agents_dir)?;
    fs::create_dir_all(&data_dir)?;
    fs::create_dir_all(&state_dir)?;

    unsafe {
        std::env::set_var("HARNX_CONFIG_DIR", &config_dir);
        std::env::set_var("HARNX_DATA_DIR", &data_dir);
        std::env::set_var("HARNX_STATE_DIR", &state_dir);
    }
    let result = f(&config_dir);
    unsafe {
        std::env::remove_var("HARNX_CONFIG_DIR");
        std::env::remove_var("HARNX_DATA_DIR");
        std::env::remove_var("HARNX_STATE_DIR");
    }

    let _ = fs::remove_dir_all(&data_dir);
    let _ = fs::remove_dir_all(&state_dir);
    let cleanup_result = fs::remove_dir_all(&config_dir);
    match (result, cleanup_result) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(err)) => Err(err.into()),
        (Err(err), Ok(())) => Err(err),
        (Err(err), Err(cleanup_err)) => Err(err.context(format!(
            "Additionally failed to clean up test config dir '{}': {cleanup_err}",
            config_dir.display()
        ))),
    }
}

fn init_test_agent(agent_name: &str, content: &str, files: &[(&str, &str)]) -> Result<Agent> {
    with_test_config_dir(|config_dir| {
        let agents_dir = config_dir.join("agents");
        fs::write(agents_dir.join(format!("{agent_name}.md")), content)?;

        for (relative_path, file_content) in files {
            let path = agents_dir.join(relative_path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(path, file_content)?;
        }

        let config = GlobalConfig::default();
        let runtime = tokio::runtime::Runtime::new()?;
        runtime.block_on(super::init(&config, agent_name, create_abort_signal()))
    })
}

fn make_tool_declaration(name: &str, description: &str) -> crate::tool::ToolDeclaration {
    crate::tool::ToolDeclaration {
        name: name.to_string(),
        description: description.to_string(),
        parameters: Default::default(),
        mcp_tool_name: None,
        mcp_server_name: None,
        call_template: None,
        result_template: None,
    }
}

fn make_agent_with_tools(prompt: &str, tools: Vec<crate::tool::ToolDeclaration>) -> Agent {
    let mut agent = Agent::new(AgentConfig::from_markdown("test", prompt).unwrap());
    agent
        .config
        .set_tools(crate::tool::Tools::init_from_mcp(if tools.is_empty() {
            None
        } else {
            Some(tools)
        }));
    agent
}

/// Build a single-variable agent markdown body with the given `path:` (and an
/// optional `default:`) and init it through [`init_test_agent`].
fn init_agent_with_path_variable(
    name: &str,
    path: &str,
    default: Option<&str>,
    files: &[(&str, &str)],
) -> Result<Agent> {
    let default_line = default
        .map(|d| format!("    default: {d}\n"))
        .unwrap_or_default();
    let content = format!(
        "---\nvariables:\n  - name: prompt\n    description: Shared prompt\n{default_line}    path: {path}\n---\nYou are a test agent.\n"
    );
    init_test_agent(name, &content, files)
}

/// Assert that the single defined variable's resolved default equals `expected`.
fn assert_path_variable_default(agent: &Agent, expected: &str) {
    assert_eq!(
        agent.defined_variables()[0].default.as_deref(),
        Some(expected)
    );
}

/// Assert that initializing an agent with the given `path:` variable fails, and
/// that the error message mentions the variable name, the path, and "not allowed".
fn assert_path_variable_rejected(name: &str, path: &str) {
    let error = init_agent_with_path_variable(name, path, None, &[]).unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("prompt"));
    assert!(message.contains(path));
    assert!(message.contains("not allowed"));
}

#[test]
fn test_agent_from_markdown_full() {
    let content = "---\nmodel: openai:gpt-4o\ntemperature: 0.7\ntop_p: 0.9\nuse_tools: fs,web_search\ndescription: A test agent\nversion: '1.0'\n---\nYou are a helpful test agent.";
    let agent = AgentConfig::from_markdown("test-agent", content).unwrap();
    assert_eq!(agent.name(), "test-agent");
    assert_eq!(agent.model_id(), Some("openai:gpt-4o"));
    assert_eq!(agent.temperature(), Some(0.7));
    assert_eq!(agent.top_p(), Some(0.9));
    assert_eq!(
        agent.use_tools(),
        Some(vec!["fs".to_string(), "web_search".to_string()])
    );
    assert!(agent
        .interpolated_instructions()
        .unwrap()
        .contains("You are a helpful test agent"));
}

#[test]
fn test_agent_from_markdown_minimal() {
    let content = "Just instructions, no front-matter.";
    let agent = AgentConfig::from_markdown("minimal", content).unwrap();
    assert_eq!(agent.name(), "minimal");
    assert!(agent.model_id().is_none());
    assert!(agent.temperature().is_none());
    assert_eq!(
        agent.interpolated_instructions().unwrap(),
        "Just instructions, no front-matter."
    );
}

#[test]
fn test_agent_from_markdown_empty_body() {
    let content = "---\nmodel: openai:gpt-4o\ntemperature: 0.5\n---\n";
    let agent = AgentConfig::from_markdown("empty-body", content).unwrap();
    assert_eq!(agent.name(), "empty-body");
    assert_eq!(agent.model_id(), Some("openai:gpt-4o"));
    assert!(agent.interpolated_instructions().unwrap().is_empty());
}

#[test]
fn test_agent_set_name() {
    let mut agent = AgentConfig::from_prompt("You are a test agent.");
    assert_eq!(agent.name(), "%%");
    agent.set_name("new-name");
    assert_eq!(agent.name(), "new-name");
}

#[test]
fn test_agent_from_prompt() {
    let agent = AgentConfig::from_prompt("You are a pirate");
    assert_eq!(agent.name(), "%%");
    assert!(agent
        .interpolated_instructions()
        .unwrap()
        .contains("You are a pirate"));
    assert!(agent.model_id().is_none());
    assert!(agent.temperature().is_none());
}

#[test]
fn test_agent_builtin_create_title() {
    let agent = super::builtin("%create-title%").unwrap();
    assert_eq!(agent.name(), "%create-title%");
    assert!(!agent.interpolated_instructions().unwrap().is_empty());
    assert!(agent
        .interpolated_instructions()
        .unwrap()
        .contains("concise"));
}

#[test]
fn test_agent_builtin_unknown() {
    let result = super::builtin("unknown-agent");
    assert!(result.is_err());
}

#[test]
fn test_agent_from_markdown_with_use_tools() {
    let content = "---\nuse_tools: fs_*,bash_exec\n---\nHelp with files.";
    let agent = AgentConfig::from_markdown("tools-agent", content).unwrap();
    assert_eq!(
        agent.use_tools(),
        Some(vec!["fs_*".to_string(), "bash_exec".to_string()])
    );
}

#[test]
fn test_agent_compaction_agent_set() {
    let content = "---\ncompaction_agent: my-compactor\n---\nYou are a test agent.";
    let agent = AgentConfig::from_markdown("test-agent", content).unwrap();
    assert_eq!(agent.compaction_agent(), Some("my-compactor"));
}

#[test]
fn test_agent_compaction_agent_unset() {
    let content = "---\nmodel: openai:gpt-4o\n---\nYou are a test agent.";
    let agent = AgentConfig::from_markdown("test-agent", content).unwrap();
    assert!(agent.compaction_agent().is_none());
}

#[test]
fn test_agent_compaction_agent_roundtrip() {
    let content =
        "---\ncompaction_agent: my-compactor\nmodel: openai:gpt-4o\n---\nYou are a test agent.";
    let agent = AgentConfig::from_markdown("test-agent", content).unwrap();

    // Export and re-parse
    let exported = agent.export().unwrap();
    let reparsed = AgentConfig::from_markdown("test-agent", &exported).unwrap();

    assert_eq!(reparsed.compaction_agent(), Some("my-compactor"));
    assert_eq!(reparsed.model_id(), Some("openai:gpt-4o"));
}

/// The system prompt must NOT enumerate the agent's tools. The model receives
/// tool definitions via the API `tools` field (filtered by `use_tools` in
/// `Config::tool_declarations_for_use_tools`); rendering an unfiltered text
/// list here duplicated those definitions and leaked tools from other packages
/// into the prompt.
#[test]
fn test_system_text_excludes_tool_summary() {
    let agent = make_agent_with_tools(
        "You are a helpful assistant.",
        vec![
            make_tool_declaration("tool_a", "Description A"),
            make_tool_declaration("tool_b", "Description B"),
        ],
    );

    let text = agent.system_text().unwrap();

    assert_eq!(text, "You are a helpful assistant.");
    assert!(!text.contains("tool_a"));
    assert!(!text.contains("Description A"));
}

#[test]
fn test_build_messages_excludes_tool_summary() {
    let config = GlobalConfig::default();
    let agent = make_agent_with_tools(
        "System prompt.",
        vec![make_tool_declaration("tool_a", "Description A")],
    );
    let input = crate::config::input::from_str(&config, "Real input", Some(agent));

    let messages = input.agent().build_messages(&input).unwrap();

    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, MessageRole::System);
    assert_eq!(messages[0].content.to_text(), "System prompt.");
    assert!(!messages[0].content.to_text().contains("tool_a"));
    assert_eq!(messages[1].content.to_text(), "Real input");
}

#[test]
fn test_export_does_not_contain_tool_text() {
    let agent = make_agent_with_tools(
        "You are a helpful assistant.",
        vec![make_tool_declaration("my_tool", "Tool description")],
    );

    let exported = agent.export().unwrap();

    assert!(!exported.contains("my_tool"));
    assert!(!exported.contains("Tool description"));
    assert!(exported.contains("You are a helpful assistant."));
}

#[test]
fn test_build_messages_always_uses_system_and_user_format() {
    let config = GlobalConfig::default();
    let agent = Agent::new(AgentConfig::from_prompt(
        "System message\n__INPUT__\n\n### INPUT:\nExample input\n### OUTPUT:\nExample output",
    ));
    let input = crate::config::input::from_str(&config, "Real input", Some(agent));

    let messages = input.agent().build_messages(&input).unwrap();

    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, MessageRole::System);
    assert_eq!(messages[1].role, MessageRole::User);
    assert_eq!(
        messages[0].content.to_text(),
        "System message\n__INPUT__\n\n### INPUT:\nExample input\n### OUTPUT:\nExample output"
    );
    assert_eq!(messages[1].content.to_text(), "Real input");
}

#[test]
fn test_agent_variable_path_deserialization() {
    let yaml = r#"name: prompt
description: Shared prompt
path: shared/prompt.md
"#;

    let variable: AgentVariable = serde_yaml::from_str(yaml).unwrap();

    assert_eq!(variable.name, "prompt");
    assert_eq!(variable.description, "Shared prompt");
    assert_eq!(variable.path.as_deref(), Some("shared/prompt.md"));
    assert!(variable.default.is_none());
    assert!(variable.value.is_empty());
}

#[test]
fn test_agent_variable_path_serialization() {
    let variable = AgentVariable {
        name: "prompt".to_string(),
        description: "Shared prompt".to_string(),
        default: None,
        path: Some("shared/prompt.md".to_string()),
        value: "runtime-only".to_string(),
    };

    let yaml = serde_yaml::to_string(&variable).unwrap();
    let round_trip: AgentVariable = serde_yaml::from_str(&yaml).unwrap();

    assert!(yaml.contains("path: shared/prompt.md"));
    assert!(!yaml.contains("value:"));
    assert_eq!(round_trip.name, "prompt");
    assert_eq!(round_trip.description, "Shared prompt");
    assert_eq!(round_trip.path.as_deref(), Some("shared/prompt.md"));
    assert!(round_trip.default.is_none());
    assert!(round_trip.value.is_empty());
}

#[test]
fn test_agent_variable_without_path() {
    let yaml = r#"name: prompt
description: Shared prompt
"#;

    let variable: AgentVariable = serde_yaml::from_str(yaml).unwrap();

    assert_eq!(variable.name, "prompt");
    assert_eq!(variable.description, "Shared prompt");
    assert!(variable.path.is_none());
    assert!(variable.default.is_none());
    assert!(variable.value.is_empty());
}

#[test]
fn test_agent_variable_with_path() {
    let agent = init_agent_with_path_variable(
        "path-variable",
        "shared/prompt.md",
        None,
        &[("shared/prompt.md", "Loaded prompt")],
    )
    .unwrap();

    assert_path_variable_default(&agent, "Loaded prompt");
}

#[test]
fn test_agent_variable_path_missing_file() {
    let error =
        init_agent_with_path_variable("missing-path-variable", "shared/missing.md", None, &[])
            .unwrap_err();

    let message = format!("{error:#}");
    assert!(message.contains("prompt"));
    assert!(message.contains("shared/missing.md"));
}

#[test]
fn test_agent_variable_path_traversal_rejected() {
    assert_path_variable_rejected("traversal-path-variable", "../../../etc/passwd");
}

#[test]
fn test_agent_variable_path_absolute_rejected() {
    assert_path_variable_rejected("absolute-path-variable", "/etc/passwd");
}

#[test]
fn test_agent_variable_path_empty_file() {
    let agent = init_agent_with_path_variable(
        "empty-path-variable",
        "shared/empty.md",
        None,
        &[("shared/empty.md", "")],
    )
    .unwrap();

    assert_path_variable_default(&agent, "");
}

#[test]
fn test_agent_variable_path_and_default_uses_path() {
    let agent = init_agent_with_path_variable(
        "path-and-default-variable",
        "shared/prompt.md",
        Some("Inline prompt"),
        &[("shared/prompt.md", "Loaded from file")],
    )
    .unwrap();

    assert_path_variable_default(&agent, "Loaded from file");
}

#[test]
fn test_agent_variable_path_nested_relative_file() {
    let agent = init_agent_with_path_variable(
        "nested-relative-path-variable",
        "shared/nested/prompt.md",
        None,
        &[("shared/nested/prompt.md", "Nested prompt")],
    )
    .unwrap();

    assert_path_variable_default(&agent, "Nested prompt");
}

/// Regression for: `harnx -a pkg/agent` and `.agent pkg/agent` would load
/// the file at `packages/<pkg>/agents/<stem>.md` but call `load(path)` —
/// which derives the agent name from the file stem alone, dropping the
/// `<pkg>/` qualifier. As a result the loaded agent reported its name as
/// the bare stem (so it looked like a top-level agent had been selected),
/// `pkg_from_qualified(agent.name())` returned `None`, and the package
/// transforms (patches, namespaced managers) were never applied.
#[test]
fn test_init_preserves_qualified_name_for_package_agent() {
    with_test_config_dir(|config_dir| {
        let pkg_agents_dir = config_dir.join("packages/pantheon/agents");
        fs::create_dir_all(&pkg_agents_dir)?;
        fs::write(
            pkg_agents_dir.join("sisyphus.md"),
            "---\nrole: assistant\n---\nPackage-scoped agent.",
        )?;
        let config = GlobalConfig::default();
        let runtime = tokio::runtime::Runtime::new()?;
        let agent = runtime.block_on(super::init(
            &config,
            "pantheon/sisyphus",
            create_abort_signal(),
        ))?;
        assert_eq!(agent.name(), "pantheon/sisyphus");
        Ok(())
    })
    .unwrap();
}

#[test]
fn test_list_assistant_agents_filters_by_role() {
    with_test_config_dir(|config_dir| {
        let agents_dir = config_dir.join("agents");
        fs::write(
            agents_dir.join("alpha.md"),
            "---\nrole: assistant\nmodel: openai:gpt-4o\n---\nAssistant agent.",
        )?;
        fs::write(
            agents_dir.join("beta.md"),
            "---\nrole: subagent\nmodel: openai:gpt-4o\n---\nSub-agent.",
        )?;
        fs::write(
            agents_dir.join("gamma.md"),
            "---\nrole: compaction\nmodel: openai:gpt-4o\n---\nCompaction agent.",
        )?;
        let runtime = tokio::runtime::Runtime::new()?;
        let result = runtime.block_on(list_assistant_agents());
        assert_eq!(result, vec!["alpha"]);
        Ok(())
    })
    .unwrap();
}

#[test]
fn test_list_assistant_agents_includes_no_role_field() {
    with_test_config_dir(|config_dir| {
        let agents_dir = config_dir.join("agents");
        fs::write(
            agents_dir.join("no-role.md"),
            "---\nmodel: openai:gpt-4o\n---\nNo role field.",
        )?;
        fs::write(
            agents_dir.join("explicit-subagent.md"),
            "---\nrole: subagent\n---\nSub-agent.",
        )?;
        let runtime = tokio::runtime::Runtime::new()?;
        let result = runtime.block_on(list_assistant_agents());
        assert_eq!(result, vec!["no-role"]);
        Ok(())
    })
    .unwrap();
}

#[test]
fn test_list_assistant_agents_empty_dir() {
    with_test_config_dir(|_config_dir| {
        let runtime = tokio::runtime::Runtime::new()?;
        let result = runtime.block_on(list_assistant_agents());
        assert!(result.is_empty());
        Ok(())
    })
    .unwrap();
}

#[test]
fn test_list_assistant_agents_skips_malformed() {
    with_test_config_dir(|config_dir| {
        let agents_dir = config_dir.join("agents");
        fs::write(
            agents_dir.join("broken.md"),
            "---\nmodel: [unclosed bracket\n---\nBroken agent.",
        )?;
        fs::write(
            agents_dir.join("good.md"),
            "---\nmodel: openai:gpt-4o\n---\nGood agent.",
        )?;
        let runtime = tokio::runtime::Runtime::new()?;
        let result = runtime.block_on(list_assistant_agents());
        assert_eq!(result, vec!["good"]);
        Ok(())
    })
    .unwrap();
}

#[test]
fn test_list_assistant_agents_sorted() {
    with_test_config_dir(|config_dir| {
        let agents_dir = config_dir.join("agents");
        fs::write(agents_dir.join("zebra.md"), "You are zebra.")?;
        fs::write(agents_dir.join("apple.md"), "You are apple.")?;
        fs::write(agents_dir.join("mango.md"), "You are mango.")?;
        let runtime = tokio::runtime::Runtime::new()?;
        let result = runtime.block_on(list_assistant_agents());
        assert_eq!(result, vec!["apple", "mango", "zebra"]);
        Ok(())
    })
    .unwrap();
}

use super::apply_agent_patch;
use harnx_core::package::PackagePatch;

fn make_patch(agents: Vec<&str>) -> PackagePatch {
    PackagePatch {
        agents: agents.into_iter().map(String::from).collect(),
        clients: vec![],
        mcp_servers: vec![],
    }
}

fn make_agent_config(name: &str, model: &str) -> super::AgentConfig {
    let content = format!("---\nmodel: {}\n---\nYou are a test agent.", model);
    super::AgentConfig::from_markdown(name, &content).expect("should parse agent config")
}

#[test]
fn apply_agent_patch_with_identity_expression_leaves_config_unchanged() {
    let mut config = make_agent_config("test-agent", "openai:gpt-4o");
    let original_model = config.model_id().map(String::from);
    let original_temperature = config.temperature();

    let patch = make_patch(vec!["."]);
    let result = apply_agent_patch(&mut config, "test-agent", &patch);

    assert!(result.is_ok());
    assert_eq!(config.model_id(), original_model.as_deref());
    assert_eq!(config.temperature(), original_temperature);
}

#[test]
fn apply_agent_patch_with_model_setting_expression_updates_config() {
    let mut config = make_agent_config("test-agent", "openai:gpt-4o");
    assert_eq!(config.model_id(), Some("openai:gpt-4o"));

    // Note: AgentConfig serializes model_id as "model" in JSON
    let patch = make_patch(vec![
        r#".model = "anthropic:claude-3-5-sonnet""#,
        r#".temperature = 0.7"#,
    ]);
    let result = apply_agent_patch(&mut config, "test-agent", &patch);

    assert!(result.is_ok());
    assert_eq!(config.model_id(), Some("anthropic:claude-3-5-sonnet"));
    assert_eq!(config.temperature(), Some(0.7));
}

#[test]
fn apply_agent_patch_with_empty_patches_is_noop() {
    let mut config = make_agent_config("test-agent", "openai:gpt-4o");
    let original_model = config.model_id().map(String::from);

    let patch = make_patch(vec![]);
    let result = apply_agent_patch(&mut config, "test-agent", &patch);

    assert!(result.is_ok());
    assert_eq!(config.model_id(), original_model.as_deref());
}

#[test]
fn apply_agent_patch_with_invalid_jq_expression_returns_err() {
    let mut config = make_agent_config("test-agent", "openai:gpt-4o");
    let original_model = config.model_id().map(String::from);

    // Invalid expression - unclosed string
    // Note: The field name in JSON is "model", not "model_id"
    let patch = make_patch(vec![r#".model = "unclosed"#]);
    let result = apply_agent_patch(&mut config, "test-agent", &patch);

    assert!(result.is_err());
    assert_eq!(config.model_id(), original_model.as_deref());
}

// ── Tests for render_agent_dump ──────────────────────────────────────────────

#[test]
fn render_agent_dump_returns_rendered_agent_md_for_file_agent() {
    let _guard = TEST_CONFIG_DIR_LOCK.lock().unwrap();
    let config_dir = unique_test_config_dir();
    let data_dir = config_dir.with_file_name(format!(
        "{}-data",
        config_dir.file_name().unwrap().to_string_lossy()
    ));
    let state_dir = config_dir.with_file_name(format!(
        "{}-state",
        config_dir.file_name().unwrap().to_string_lossy()
    ));
    let _config = EnvGuard::new("HARNX_CONFIG_DIR", &config_dir);
    let _data = EnvGuard::new("HARNX_DATA_DIR", &data_dir);
    let _state = EnvGuard::new("HARNX_STATE_DIR", &state_dir);
    let agents_dir = config_dir.join("agents");
    fs::create_dir_all(&agents_dir).unwrap();
    fs::create_dir_all(&data_dir).unwrap();
    fs::create_dir_all(&state_dir).unwrap();
    write_agent_dump_test_config();

    let agent_content = r#"---
model: openai:gpt-4o
use_tools:
  - "*"
variables:
  - name: project_name
    description: Project name
    default: harnx
---
Hello {{project_name}} from {{agent.name}}.
"#;
    fs::write(agents_dir.join("test-render.md"), agent_content).unwrap();

    let config = Config {
        tools: crate::tool::Tools::init_from_mcp(Some(vec![
            make_tool_declaration("fs_read", "Read files"),
            make_tool_declaration("fs_write", "Write files"),
        ])),
        ..Config::default()
    };

    let rendered = render_agent_dump(&config, "test-render")
        .expect("render_agent_dump should succeed for known agent");

    assert!(
        rendered.contains("use_tools:"),
        "should have use_tools in front-matter"
    );
    assert!(
        rendered.contains("- fs_read"),
        "should have concrete tool fs_read"
    );
    assert!(
        rendered.contains("- fs_write"),
        "should have concrete tool fs_write"
    );
    assert!(
        rendered.contains("Hello harnx from test-render."),
        "body should be interpolated"
    );
    assert!(
        !rendered.contains("{{project_name}}"),
        "raw template var should be gone"
    );
    assert!(
        !rendered.contains("\"*\""),
        "wildcard should be expanded, not preserved"
    );
}

#[test]
fn render_agent_dump_snapshot_expands_tools_and_interpolates_body() {
    let _guard = TEST_CONFIG_DIR_LOCK.lock().unwrap();
    let config_dir = unique_test_config_dir();
    let data_dir = config_dir.with_file_name(format!(
        "{}-data",
        config_dir.file_name().unwrap().to_string_lossy()
    ));
    let state_dir = config_dir.with_file_name(format!(
        "{}-state",
        config_dir.file_name().unwrap().to_string_lossy()
    ));
    let _config = EnvGuard::new("HARNX_CONFIG_DIR", &config_dir);
    let _data = EnvGuard::new("HARNX_DATA_DIR", &data_dir);
    let _state = EnvGuard::new("HARNX_STATE_DIR", &state_dir);
    let agents_dir = config_dir.join("agents");
    fs::create_dir_all(&agents_dir).unwrap();
    fs::create_dir_all(&data_dir).unwrap();
    fs::create_dir_all(&state_dir).unwrap();
    write_agent_dump_test_config();

    fs::write(
        agents_dir.join("snapshot-agent.md"),
        r#"---
model: openai:gpt-4o
use_tools:
  - "*"
variables:
  - name: project_name
    description: Project name
    default: harnx
conversation_starters:
  - Show rendered prompt
---
Project {{project_name}} belongs to {{agent.name}}.
"#,
    )
    .unwrap();

    let config = Config {
        tools: crate::tool::Tools::init_from_mcp(Some(vec![
            make_tool_declaration("alpha_tool", "Alpha tool"),
            make_tool_declaration("beta_tool", "Beta tool"),
        ])),
        ..Config::default()
    };

    let rendered = render_agent_dump(&config, "snapshot-agent").unwrap();

    insta::assert_snapshot!("render_agent_dump_snapshot", rendered);
}

#[test]
fn render_agent_dump_errors_cleanly_for_unknown_agent() {
    let config = Config::default();
    let result = render_agent_dump(&config, "nonexistent-agent-xyz");

    assert!(result.is_err(), "should error for unknown agent");
    let err = result.unwrap_err();
    let msg = format!("{:#}", err);
    assert!(
        msg.contains("not found"),
        "error message should mention 'not found', got: {msg}"
    );
}

#[test]
fn render_agent_dump_works_for_builtin_agent() {
    let config = Config::default();

    // %create-title% is a builtin agent
    let result = render_agent_dump(&config, "%create-title%");

    let rendered = result.expect("should work for builtin agent");
    assert!(rendered.contains("---"), "should have front-matter");
    // Builtin agent content should contain relevant instructions
    assert!(
        rendered.contains("title") || rendered.contains("Title"),
        "should contain title-related content"
    );
}

#[test]
fn render_agent_dump_handles_package_qualified_agent() {
    let _guard = TEST_CONFIG_DIR_LOCK.lock().unwrap();
    let config_dir = unique_test_config_dir();
    let data_dir = config_dir.with_file_name(format!(
        "{}-data",
        config_dir.file_name().unwrap().to_string_lossy()
    ));
    let state_dir = config_dir.with_file_name(format!(
        "{}-state",
        config_dir.file_name().unwrap().to_string_lossy()
    ));
    let packages_dir = config_dir.join("packages");
    let pkg_dir = packages_dir.join("mypkg");
    let agents_dir = pkg_dir.join("agents");
    fs::create_dir_all(&agents_dir).unwrap();
    fs::create_dir_all(&data_dir).unwrap();
    fs::create_dir_all(&state_dir).unwrap();

    unsafe {
        std::env::set_var("HARNX_CONFIG_DIR", &config_dir);
        std::env::set_var("HARNX_DATA_DIR", &data_dir);
        std::env::set_var("HARNX_STATE_DIR", &state_dir);
    }

    // Write a package-qualified agent
    let agent_content = r#"---
model: openai:gpt-4o
---
You are a package agent.
"#;
    fs::write(agents_dir.join("pkg-agent.md"), agent_content).unwrap();

    let config = Config::default();
    let result = render_agent_dump(&config, "mypkg/pkg-agent");

    // Cleanup
    unsafe {
        std::env::remove_var("HARNX_CONFIG_DIR");
        std::env::remove_var("HARNX_DATA_DIR");
        std::env::remove_var("HARNX_STATE_DIR");
    }
    let _ = fs::remove_dir_all(&config_dir);
    let _ = fs::remove_dir_all(&data_dir);
    let _ = fs::remove_dir_all(&state_dir);

    let rendered = result.expect("should succeed for package-qualified agent");
    assert!(
        rendered.contains("You are a package agent"),
        "should have agent body"
    );
}
