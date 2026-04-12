#![cfg(test)]

use crate::test_utils::{
    MockOpenAiScript, MockOpenAiServer, MockOpenAiToolCall, MockOpenAiTurn, TmuxHarness,
};

use anyhow::{Context, Result};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::TempDir;

const REPRO_249_MCP_TOOL_NAME: &str = "repro249_unique_mcp_tool";
const REPRO_249_MCP_TOOL_RESPONSE: &str = "repro249 fixed tool response";
const TEST_AGENT_NAME: &str = "test-agent";
const TEST_SUB_AGENT_NAME: &str = "test-sub-agent";

#[test]
fn repro_249_top_level_delegation_markers() -> Result<()> {
    if option_env!("CARGO_BIN_NAME") == Some("harnx") {
        eprintln!(
            "skipping repro_249_top_level_delegation_markers in binary test target to avoid duplicate tmux sessions"
        );
        return Ok(());
    }

    if !TmuxHarness::is_available() {
        eprintln!("skipping repro_249_top_level_delegation_markers: tmux is unavailable");
        return Ok(());
    }

    let repo_root = repo_root()?;
    let target_dir = repo_root.join("target").join("debug");
    let harnx_bin = target_dir.join(binary_name("harnx"));

    if !harnx_bin.is_file() {
        eprintln!("skipping repro_249_top_level_delegation_markers: harnx binary is missing");
        return Ok(());
    }

    let harnx_mcp_bin = target_dir.join(binary_name("harnx-mcp-repro249"));
    if !harnx_mcp_bin.is_file() {
        eprintln!(
            "skipping repro_249_top_level_delegation_markers: harnx-mcp-repro249 binary is missing"
        );
        return Ok(());
    }

    let temp = TempDir::new().context("failed to create temp dir")?;
    let mock = MockOpenAiServer::start(script())?;
    let paths = TestPaths::new(temp.path(), mock.port())?;
    write_fixture_files(&paths)?;

    let path_env = format!(
        "{}:{}",
        target_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let tmux = match TmuxHarness::new(&repo_root, 120, 35) {
        Ok(tmux) => tmux,
        Err(err) => {
            eprintln!(
                "skipping repro_249_top_level_delegation_markers: tmux is unavailable or unusable ({err:#})"
            );
            return Ok(());
        }
    };
    tmux.send_keys(&["C-l"])?;

    // Step 1: Export HARNX_CONFIG_DIR
    tmux.send_text(&format!(
        "export HARNX_CONFIG_DIR={}",
        shell_escape(paths.harnx_config_dir.to_string_lossy().as_ref())
    ))?;
    tmux.send_keys(&["Enter"])?;

    // Step 2: Export PATH and cd to repo root for subsequent commands
    //
    // Each step uses a unique marker. The wait_for predicate requires the
    // marker to appear at least twice in the pane: once in the echoed command
    // line and once as actual output, ensuring the command has finished.
    tmux.send_text(&format!(
        "export PATH={} && cd {}; printf '__READY_2__\\n'",
        shell_escape(&path_env),
        shell_escape(repo_root.to_string_lossy().as_ref())
    ))?;
    tmux.send_keys(&["Enter"])?;
    tmux.wait_for(Duration::from_secs(5), |screen| {
        count_occurrences(screen, "__READY_2__") >= 2
    })?;

    // Step 3: Run diagnostics - list agents
    tmux.send_text(&format!(
        "{} --list-agents; printf '__READY_3__\\n'",
        shell_escape(harnx_bin.to_string_lossy().as_ref())
    ))?;
    tmux.send_keys(&["Enter"])?;
    tmux.wait_for(Duration::from_secs(5), |screen| {
        count_occurrences(screen, "__READY_3__") >= 2
    })?;

    // Step 4: Run diagnostics - list models
    tmux.send_text(&format!(
        "{} --list-models; printf '__READY_4__\\n'",
        shell_escape(harnx_bin.to_string_lossy().as_ref())
    ))?;
    tmux.send_keys(&["Enter"])?;
    tmux.wait_for(Duration::from_secs(5), |screen| {
        count_occurrences(screen, "__READY_4__") >= 2
    })?;

    // Step 5: Launch the test agent
    tmux.send_text(&format!(
        "{} -a {} || echo HARNX_EXIT:$?",
        shell_escape(harnx_bin.to_string_lossy().as_ref()),
        TEST_AGENT_NAME
    ))?;
    tmux.send_keys(&["Enter"])?;

    tmux.wait_for_contains("Welcome to harnx", Duration::from_secs(15))?;
    tmux.send_text("tell me how many files are in /tmp")?;
    tmux.send_keys(&["Enter"])?;

    // Wait for delegation, child heading, and nested MCP tool call to all appear.
    let screen = tmux.wait_for(Duration::from_secs(30), |screen| {
        screen.contains(&format!("{}_session_prompt", TEST_SUB_AGENT_NAME))
            && screen.contains(&format!("> {}", TEST_SUB_AGENT_NAME))
            && nested_mcp_tool_present(screen)
    })?;

    // ── Known-good markers: delegation happened ──────────────────────────────
    assert!(
        screen.contains(&format!("{}_session_prompt", TEST_SUB_AGENT_NAME)),
        "delegation tool call not visible:\n{screen}"
    );
    assert!(
        screen.contains(&format!("> {}", TEST_SUB_AGENT_NAME)),
        "child sub-agent heading not visible:\n{screen}"
    );

    // ── Issue #249 regression assertion ───────────────────────────────────────
    // The child sub-agent internally called `repro249_unique_mcp_tool` via MCP.
    // That internal tool call should remain visible in the parent transcript.
    let nested_visible = nested_mcp_tool_present(&screen);
    eprintln!(
        "\n=== Issue #249 tmux repro result ===\n\
         delegation visible    : {}\n\
         child heading visible : {}\n\
         nested tool visible   : {} (expected: true — visible = fix confirmed)\n\
         === last screen ===\n{}\n====\n",
        screen.contains(&format!("{}_session_prompt", TEST_SUB_AGENT_NAME)),
        screen.contains(&format!("> {}", TEST_SUB_AGENT_NAME)),
        nested_visible,
        screen,
    );
    assert!(
        nested_visible,
        "nested internal MCP tool call is not visible in the parent transcript; issue #249 regressed\n{screen}"
    );

    drop(tmux);
    drop(mock);
    Ok(())
}

struct TestPaths {
    harnx_config_dir: PathBuf,
    config_path: PathBuf,
    agents_dir: PathBuf,
    port: u16,
}

impl TestPaths {
    fn new(temp_root: &Path, port: u16) -> Result<Self> {
        let harnx_config_dir = temp_root.join("harnx");
        let config_path = harnx_config_dir.join("config.yaml");
        let agents_dir = harnx_config_dir.join("agents");
        std::fs::create_dir_all(&agents_dir)?;
        Ok(Self {
            harnx_config_dir,
            config_path,
            agents_dir,
            port,
        })
    }
}

fn script() -> MockOpenAiScript {
    // The mock LLM is shared between the parent and sub-agent processes.
    // Turns are consumed in order across both processes:
    //   Turn 0 (parent)    : delegate to the sub-agent via ACP tool call
    //   Turn 1 (sub-agent) : call the MCP tool repro249_unique_mcp_tool
    //   Turn 2 (sub-agent) : summarize the MCP tool result (ends the sub-agent)
    //   Turn 3 (parent)    : final summary after delegation returns
    MockOpenAiScript {
        turns: vec![
            // Turn 0: parent delegates to sub-agent
            MockOpenAiTurn {
                text_chunks: vec!["I'll delegate this to the sub-agent.".to_string()],
                tool_calls: vec![MockOpenAiToolCall {
                    name: format!("{}_session_prompt", TEST_SUB_AGENT_NAME),
                    arguments: json!({
                        "message": format!(
                            "Call the MCP tool named {REPRO_249_MCP_TOOL_NAME} and report its result."
                        )
                    }),
                    id: Some("call_delegate_1".to_string()),
                }],
            },
            // Turn 1: sub-agent calls the MCP tool
            MockOpenAiTurn {
                text_chunks: vec!["Let me call the tool.".to_string()],
                tool_calls: vec![MockOpenAiToolCall {
                    name: REPRO_249_MCP_TOOL_NAME.to_string(),
                    arguments: json!({}),
                    id: Some("call_mcp_1".to_string()),
                }],
            },
            // Turn 2: sub-agent summarizes after getting MCP tool result
            MockOpenAiTurn {
                text_chunks: vec![format!(
                    "The tool returned: {REPRO_249_MCP_TOOL_RESPONSE}"
                )],
                tool_calls: vec![],
            },
            // Turn 3: parent summarizes after delegation returns
            MockOpenAiTurn {
                text_chunks: vec![format!(
                    "The child used {REPRO_249_MCP_TOOL_NAME} and got: {REPRO_249_MCP_TOOL_RESPONSE}"
                )],
                tool_calls: vec![],
            },
        ],
        fallback_text: "No more scripted responses.".to_string(),
        chunk_delay_ms: 0,
    }
}

fn write_fixture_files(paths: &TestPaths) -> Result<()> {
    std::fs::create_dir_all(&paths.harnx_config_dir)?;

    let fake_mcp_server = repo_root()?
        .join("target")
        .join("debug")
        .join(binary_name("harnx-mcp-repro249"));

    let harnx_bin = repo_root()?
        .join("target")
        .join("debug")
        .join(binary_name("harnx"));

    let config = format!(
        "save: false\nclients:\n  - type: openai-compatible\n    name: mock-llm\n    api_base: http://127.0.0.1:{}/v1\n    api_key: dummy\n    models:\n      - name: test\n        max_input_tokens: 32000\n        max_output_tokens: 4096\n        supports_tool_use: true\nmcp_servers:\n  - name: repro249\n    command: {}\n    enabled: true\n    rename_tools:\n      {}: {}\nacp_servers:\n  - name: {}\n    command: {}\n    args: [\"--acp\", {}]\n    enabled: true\n",
        paths.port,
        yaml_escape(fake_mcp_server.to_string_lossy().as_ref()),
        REPRO_249_MCP_TOOL_NAME,
        REPRO_249_MCP_TOOL_NAME,
        TEST_SUB_AGENT_NAME,
        yaml_escape(harnx_bin.to_string_lossy().as_ref()),
        yaml_escape(TEST_SUB_AGENT_NAME),
    );
    std::fs::write(&paths.config_path, config)?;
    std::fs::write(
        paths.agents_dir.join(format!("{}.md", TEST_AGENT_NAME)),
        format!(
            "---\nname: {}\nmodel: mock-llm:test\nuse_tools: {}_session_prompt\n---\nYou are {}. Delegate the task to {} and then summarize the result.\n",
            TEST_AGENT_NAME,
            TEST_SUB_AGENT_NAME,
            TEST_AGENT_NAME,
            TEST_SUB_AGENT_NAME,
        ),
    )?;
    std::fs::write(
        paths.agents_dir.join(format!("{}.md", TEST_SUB_AGENT_NAME)),
        format!(
            "---\nname: {}\nmodel: mock-llm:test\nuse_tools: {}\n---\nYou are {}. Always call the MCP tool named {REPRO_249_MCP_TOOL_NAME} before replying.\n",
            TEST_SUB_AGENT_NAME,
            REPRO_249_MCP_TOOL_NAME,
            TEST_SUB_AGENT_NAME,
        ),
    )?;
    Ok(())
}

#[allow(dead_code)]
fn usage_line_present(screen: &str) -> bool {
    let compact = screen.replace('\n', " ");
    let Some(in_pos) = compact.find(" in ") else {
        return false;
    };
    let Some(out_pos) = compact[in_pos..].find(" out ") else {
        return false;
    };
    compact[in_pos + out_pos + 5..]
        .chars()
        .any(|c| c.is_ascii_digit())
}

fn nested_mcp_tool_present(screen: &str) -> bool {
    // The nested tool call must appear as a real tool-call row in the parent
    // transcript, NOT just as text inside a response string.
    // A real tool call row looks like "->️ repro249_unique_mcp_tool"
    screen.contains(&format!("->️ {}", REPRO_249_MCP_TOOL_NAME))
}

fn repo_root() -> Result<PathBuf> {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .canonicalize()
        .context("failed to resolve repo root")
}

fn binary_name(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    }
}

fn shell_escape(input: &str) -> String {
    if input.is_empty() {
        return "''".to_string();
    }
    // Escape single quotes for shell by replacing ' with '"'"'
    format!("'{}'", input.replace('\'', "'\"'\"'"))
}

fn count_occurrences(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

fn yaml_escape(input: &str) -> String {
    // For YAML string values, wrap in double quotes and escape internal double quotes and backslashes
    if input.is_empty() {
        return "\"\"".to_string();
    }
    // Simple escaping: escape backslashes first, then double quotes
    let escaped = input.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{}\"", escaped)
}
