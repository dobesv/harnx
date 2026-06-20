//! Tests for render_agent_dump isolated from broader agent tests for code health.
#![cfg(test)]

use super::*;
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
        "harnx-agent-dump-test-{}-{timestamp}",
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

fn render_test_agent_dump_fixture(
    agent_name: &str,
    agent_content: &str,
    tool_specs: &[(&str, &str)],
) -> String {
    with_test_config_dir(|config_dir| {
        let agents_dir = config_dir.join("agents");
        fs::create_dir_all(&agents_dir).unwrap();
        write_agent_dump_test_config();
        fs::write(agents_dir.join(format!("{agent_name}.md")), agent_content).unwrap();

        let config = Config {
            tools: crate::tool::Tools::init_from_mcp(Some(
                tool_specs
                    .iter()
                    .map(|(name, description)| make_tool_declaration(name, description))
                    .collect(),
            )),
            ..Config::default()
        };

        render_agent_dump(&config, agent_name)
    })
    .expect("render_agent_dump should succeed for known agent")
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

#[test]
fn render_agent_dump_returns_rendered_agent_md_for_file_agent() {
    let rendered = render_test_agent_dump_fixture(
        "test-render",
        r#"---
model: openai:gpt-4o
use_tools:
  - "*"
variables:
  - name: project_name
    description: Project name
    default: harnx
---
Hello {{project_name}} from {{agent.name}}.
"#,
        &[("fs_read", "Read files"), ("fs_write", "Write files")],
    );

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
        "wildcard tool marker should be expanded away"
    );
}

#[test]
fn render_agent_dump_snapshot_expands_tools_and_interpolates_body() {
    let rendered = render_test_agent_dump_fixture(
        "snapshot-agent",
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
        &[("alpha_tool", "Alpha tool"), ("beta_tool", "Beta tool")],
    );

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

    let result = render_agent_dump(&config, "%create-title%");

    let rendered = result.expect("should work for builtin agent");
    assert!(rendered.contains("---"), "should have front-matter");
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

    let agent_content = r#"---
model: openai:gpt-4o
---
You are a package agent.
"#;
    fs::write(agents_dir.join("pkg-agent.md"), agent_content).unwrap();

    let config = Config::default();
    let result = render_agent_dump(&config, "mypkg/pkg-agent");

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

// ── export_rendered regression tests (#886 instructions field) ───────────────

/// Regression test for #886: export_rendered must clear the `instructions` field
/// so the rendered body is the single source of the prompt. Without this fix,
/// interpolated_instructions() prefers instructions over prompt, reintroducing
/// raw template vars on a from_markdown round-trip.
#[test]
fn export_rendered_clears_instructions_to_prevent_template_reintroduction() {
    use harnx_core::agent_config::AgentConfig;

    // Agent with instructions field containing a template var
    // Note: variables require a description field
    let agent_md = r#"---
model: openai:gpt-4o
variables:
  - name: greeting
    description: Greeting text
    default: Hello
instructions: "{{greeting}} from instructions."
---
Body content here.
"#;
    let mut agent_config = AgentConfig::from_markdown("test", agent_md).unwrap();

    // Set shared_variables so template interpolation works
    let mut vars = harnx_core::agent_config::AgentVariables::default();
    vars.insert("greeting".to_string(), "Hello".to_string());
    agent_config.set_shared_variables(vars);

    // Call export_rendered
    let rendered = agent_config
        .export_rendered(&["fs_read".to_string()])
        .unwrap();

    // Verify NO raw template syntax remains
    assert!(
        !rendered.contains("{{greeting}}"),
        "raw template var should be gone"
    );

    // Verify NO instructions: field in output (it was cleared)
    assert!(
        !rendered.contains("instructions:"),
        "instructions field should be omitted"
    );

    // Verify the BODY is the interpolated content
    assert!(
        rendered.contains("Hello from instructions."),
        "body should have interpolated content"
    );

    // Round-trip: parse the rendered output
    let reparsed = AgentConfig::from_markdown("test-roundtrip", &rendered).unwrap();

    // Verify the reparsed agent's interpolated_instructions matches what we wrote (no raw template)
    let reparsed_instructions = reparsed.interpolated_instructions().unwrap();
    assert!(
        !reparsed_instructions.contains("{{greeting}}"),
        "round-trip should not have raw template"
    );
}

/// Regression test: agent with use_tools omitted entirely should handle gracefully
/// (empty use_tools → empty list, no implicit tools).
#[test]
fn export_rendered_handles_missing_use_tools_gracefully() {
    use harnx_core::agent_config::AgentConfig;

    // Agent WITHOUT use_tools field
    let agent_md = r#"---
model: openai:gpt-4o
---
Simple agent prompt.
"#;
    let agent_config = AgentConfig::from_markdown("test", agent_md).unwrap();

    // Call export_rendered with empty tools (simulating no use_tools)
    let rendered = agent_config.export_rendered(&[]).unwrap();

    // Verify it renders without panic
    assert!(
        rendered.contains("Simple agent prompt."),
        "should have body content"
    );

    // Should have use_tools: [] in front-matter (explicit empty)
    assert!(rendered.contains("use_tools:"), "should have use_tools key");
}

/// Regression test for variable overlay: CLI-provided agent_variables should
/// overlay on top of defined-variable defaults, not replace them entirely.
#[test]
fn render_agent_dump_overlays_cli_variables_on_defaults() {
    let rendered = with_test_config_dir(|config_dir| {
        let agents_dir = config_dir.join("agents");
        fs::create_dir_all(&agents_dir).unwrap();
        write_agent_dump_test_config();

        // Agent with TWO defined variables, each with defaults
        let agent_content = r#"---
model: openai:gpt-4o
use_tools:
  - "*"
variables:
  - name: project_name
    description: Project name
    default: harnx
  - name: user_name
    description: User name
    default: "default-user"
---
Hello {{project_name}} from {{user_name}}.
"#;
        fs::write(agents_dir.join("overlay-test.md"), agent_content).unwrap();

        // Create a config that overrides ONE variable via CLI
        // (simulates --agent-variable user_name=alice)
        let config = Config {
            tools: crate::tool::Tools::init_from_mcp(Some(
                [("fs_read", "Read"), ("fs_write", "Write")]
                    .iter()
                    .map(|(n, d)| make_tool_declaration(n, d))
                    .collect(),
            )),
            // Override user_name but leave project_name unspecified
            agent_variables: Some({
                let mut vars = AgentVariables::default();
                vars.insert("user_name".to_string(), "alice".to_string());
                vars
            }),
            ..Config::default()
        };

        render_agent_dump(&config, "overlay-test")
    })
    .unwrap();

    // Both variables should interpolate:
    // - project_name uses its default ("harnx") because CLI didn't override it
    // - user_name uses CLI value ("alice")
    assert!(
        rendered.contains("Hello harnx from alice."),
        "both defaults should be present (project_name default, user_name from CLI): {rendered}"
    );

    // Raw templates should be gone
    assert!(
        !rendered.contains("{{project_name}}"),
        "project_name should be interpolated"
    );
    assert!(
        !rendered.contains("{{user_name}}"),
        "user_name should be interpolated"
    );
}
