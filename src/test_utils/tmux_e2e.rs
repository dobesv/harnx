#![cfg(test)]

use crate::acp::AcpServerConfig;
use crate::mcp::McpServerConfig;
use crate::test_utils::{
    MockOpenAiScript, MockOpenAiServer, MockOpenAiToolCall, MockOpenAiTurn, TmuxHarness,
};

use anyhow::{Context, Result};
use insta::assert_snapshot;
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
                error: None,
            },
            // Turn 1: sub-agent calls the MCP tool
            MockOpenAiTurn {
                text_chunks: vec!["Let me call the tool.".to_string()],
                tool_calls: vec![MockOpenAiToolCall {
                    name: REPRO_249_MCP_TOOL_NAME.to_string(),
                    arguments: json!({}),
                    id: Some("call_mcp_1".to_string()),
                }],
                error: None,
            },
            // Turn 2: sub-agent summarizes after getting MCP tool result
            MockOpenAiTurn {
                text_chunks: vec![format!(
                    "The tool returned: {REPRO_249_MCP_TOOL_RESPONSE}"
                )],
                tool_calls: vec![],
                error: None,
            },
            // Turn 3: parent summarizes after delegation returns
            MockOpenAiTurn {
                text_chunks: vec![format!(
                    "The child used {REPRO_249_MCP_TOOL_NAME} and got: {REPRO_249_MCP_TOOL_RESPONSE}"
                )],
                tool_calls: vec![],
                error: None,
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

    std::fs::write(
        &paths.config_path,
        "save: false
",
    )?;

    let clients_dir = paths.harnx_config_dir.join("clients");
    let mcp_servers_dir = paths.harnx_config_dir.join("mcp_servers");
    let acp_servers_dir = paths.harnx_config_dir.join("acp_servers");
    std::fs::create_dir_all(&clients_dir)?;
    std::fs::create_dir_all(&mcp_servers_dir)?;
    std::fs::create_dir_all(&acp_servers_dir)?;
    let client = serde_yaml::Value::Mapping(serde_yaml::Mapping::from_iter([
        (
            serde_yaml::Value::String("type".to_string()),
            serde_yaml::Value::String("openai-compatible".to_string()),
        ),
        (
            serde_yaml::Value::String("name".to_string()),
            serde_yaml::Value::String("mock-llm".to_string()),
        ),
        (
            serde_yaml::Value::String("api_base".to_string()),
            serde_yaml::Value::String(format!("http://127.0.0.1:{}/v1", paths.port)),
        ),
        (
            serde_yaml::Value::String("api_key".to_string()),
            serde_yaml::Value::String("dummy".to_string()),
        ),
        (
            serde_yaml::Value::String("models".to_string()),
            serde_yaml::Value::Sequence(vec![serde_yaml::Value::Mapping(
                serde_yaml::Mapping::from_iter([
                    (
                        serde_yaml::Value::String("name".to_string()),
                        serde_yaml::Value::String("test".to_string()),
                    ),
                    (
                        serde_yaml::Value::String("max_input_tokens".to_string()),
                        serde_yaml::Value::Number(32000.into()),
                    ),
                    (
                        serde_yaml::Value::String("max_output_tokens".to_string()),
                        serde_yaml::Value::Number(4096.into()),
                    ),
                    (
                        serde_yaml::Value::String("supports_tool_use".to_string()),
                        serde_yaml::Value::Bool(true),
                    ),
                ]),
            )]),
        ),
    ]));
    std::fs::write(
        clients_dir.join("mock-llm.yaml"),
        serde_yaml::to_string(&client)?,
    )?;

    let mut rename_tools = std::collections::HashMap::new();
    rename_tools.insert(
        REPRO_249_MCP_TOOL_NAME.to_string(),
        REPRO_249_MCP_TOOL_NAME.to_string(),
    );
    let mcp_server = McpServerConfig {
        name: "repro249".to_string(),
        command: fake_mcp_server.to_string_lossy().into_owned(),
        args: vec![],
        env: Default::default(),
        roots: vec![],
        enabled: true,
        description: None,
        rename_tools,
    };
    std::fs::write(
        mcp_servers_dir.join("repro249.yaml"),
        serde_yaml::to_string(&mcp_server)?,
    )?;

    let acp_server = AcpServerConfig {
        name: TEST_SUB_AGENT_NAME.to_string(),
        command: harnx_bin.to_string_lossy().into_owned(),
        args: vec!["--acp".to_string(), TEST_SUB_AGENT_NAME.to_string()],
        env: Default::default(),
        enabled: true,
        description: None,
        idle_timeout_secs: 300,
        operation_timeout_secs: 3600,
    };
    std::fs::write(
        acp_servers_dir.join(format!("{}.yaml", TEST_SUB_AGENT_NAME)),
        serde_yaml::to_string(&acp_server)?,
    )?;
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

fn nested_mcp_tool_present(screen: &str) -> bool {
    // The nested tool call must appear as a real tool-call row in the parent
    // transcript, NOT just as text inside a response string.
    // A real tool call row looks like "->️ repro249_unique_mcp_tool"
    screen.contains(&format!("->️ {}", REPRO_249_MCP_TOOL_NAME))
}

fn normalize_screen(screen: &str) -> String {
    screen
        .lines()
        .map(|line| line.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
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

const HANDOFF_AGENT_NAME: &str = "handoff-agent";
const HANDOFF_SUB_AGENT_NAME: &str = "handoff-sub-agent";
const HANDOFF_SESSION_ID: &str = "handoff-session-1";
const HANDOFF_FINAL_RESPONSE: &str = "I took over this session and counted 7 files in /tmp.";
const HANDOFF_REUSE_FOLLOWUP_RESPONSE: &str =
    "I remember the earlier /tmp answer: 7 files, and this is follow-up #2 in the same handed-off session.";

#[test]
fn issue_149_interactive_handoff_switches_control() -> Result<()> {
    let session =
        match setup_handoff_tmux_session("issue_149_interactive_handoff_switches_control")? {
            Some(session) => session,
            None => return Ok(()),
        };

    let HandoffTmuxSession {
        tmux,
        mock,
        harnx_bin,
        ..
    } = session;

    tmux.send_text(&format!(
        "{} -a {} || echo HARNX_EXIT:$?",
        shell_escape(harnx_bin.to_string_lossy().as_ref()),
        HANDOFF_AGENT_NAME
    ))?;
    tmux.send_keys(&["Enter"])?;

    tmux.wait_for_contains("Welcome to harnx", Duration::from_secs(15))?;
    tmux.send_text("tell me how many files are in /tmp")?;
    tmux.send_keys(&["Enter"])?;

    let screen = tmux.wait_for(Duration::from_secs(30), |screen| {
        screen.contains(&format!("{}_session_handoff", HANDOFF_SUB_AGENT_NAME))
            && screen.contains(HANDOFF_FINAL_RESPONSE)
    })?;

    assert!(
        screen.contains(&format!("{}_session_handoff", HANDOFF_SUB_AGENT_NAME)),
        "handoff tool call not visible:
{screen}"
    );
    assert!(
        screen.contains(HANDOFF_SESSION_ID),
        "handed-off session id not visible anywhere in transcript:
{screen}"
    );
    assert!(
        screen.contains(HANDOFF_FINAL_RESPONSE),
        "handed-off agent response not visible:
{screen}"
    );

    let normalized = normalize_screen(&screen);
    assert_snapshot!("issue_149_interactive_handoff_switches_control", normalized);

    drop(tmux);
    drop(mock);
    Ok(())
}

#[test]
fn issue_149_interactive_handoff_reuses_session_across_followups() -> Result<()> {
    let session = match setup_handoff_tmux_session(
        "issue_149_interactive_handoff_reuses_session_across_followups",
    )? {
        Some(session) => session,
        None => return Ok(()),
    };

    let HandoffTmuxSession {
        tmux,
        mock,
        harnx_bin,
        ..
    } = session;

    tmux.send_text(&format!(
        "{} -a {} || echo HARNX_EXIT:$?",
        shell_escape(harnx_bin.to_string_lossy().as_ref()),
        HANDOFF_AGENT_NAME
    ))?;
    tmux.send_keys(&["Enter"])?;

    tmux.wait_for_contains("Welcome to harnx", Duration::from_secs(15))?;
    tmux.send_text("tell me how many files are in /tmp")?;
    tmux.send_keys(&["Enter"])?;

    let first_screen = tmux.wait_for(Duration::from_secs(30), |screen| {
        screen.contains(&format!("{}_session_handoff", HANDOFF_SUB_AGENT_NAME))
            && screen.contains(HANDOFF_FINAL_RESPONSE)
    })?;

    assert!(
        first_screen.contains(HANDOFF_SESSION_ID),
        "initial handed-off session id not visible anywhere in transcript:
{first_screen}"
    );

    tmux.send_text(
        "what was the earlier /tmp answer, and are you still in that same handed-off session?",
    )?;
    tmux.send_keys(&["Enter"])?;

    let second_screen = tmux.wait_for(Duration::from_secs(30), |screen| {
        screen.contains(HANDOFF_REUSE_FOLLOWUP_RESPONSE)
    })?;

    assert!(
        second_screen.contains(HANDOFF_FINAL_RESPONSE),
        "follow-up transcript no longer shows the initial handed-off answer:
{second_screen}"
    );
    assert!(
        second_screen.contains(HANDOFF_REUSE_FOLLOWUP_RESPONSE),
        "follow-up response showing session reuse not visible:
{second_screen}"
    );
    assert_eq!(
        count_occurrences(&second_screen, &format!("{}_session_handoff", HANDOFF_SUB_AGENT_NAME)),
        1,
        "expected exactly one handoff tool call in the transcript, indicating reuse of the existing delegated session:
{second_screen}"
    );

    let normalized = normalize_screen(&second_screen);
    assert_snapshot!(
        "issue_149_interactive_handoff_reuses_session_across_followups",
        normalized
    );

    drop(tmux);
    drop(mock);
    Ok(())
}

struct HandoffTmuxSession {
    _temp: TempDir,
    mock: MockOpenAiServer,
    tmux: TmuxHarness,
    harnx_bin: PathBuf,
}

fn setup_handoff_tmux_session(test_name: &str) -> Result<Option<HandoffTmuxSession>> {
    if option_env!("CARGO_BIN_NAME") == Some("harnx") {
        eprintln!("skipping {test_name} in binary test target to avoid duplicate tmux sessions");
        return Ok(None);
    }

    if !TmuxHarness::is_available() {
        eprintln!("skipping {test_name}: tmux is unavailable");
        return Ok(None);
    }

    let repo_root = repo_root()?;
    let target_dir = repo_root.join("target").join("debug");
    let harnx_bin = target_dir.join(binary_name("harnx"));

    if !harnx_bin.is_file() {
        eprintln!("skipping {test_name}: harnx binary is missing");
        return Ok(None);
    }

    let temp = TempDir::new().context("failed to create temp dir")?;
    let mock = MockOpenAiServer::start(handoff_script())?;
    let paths = TestPaths::new(temp.path(), mock.port())?;
    write_handoff_fixture_files(&paths)?;

    let path_env = format!(
        "{}:{}",
        target_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let tmux = match TmuxHarness::new(&repo_root, 120, 35) {
        Ok(tmux) => tmux,
        Err(err) => {
            eprintln!("skipping {test_name}: tmux is unavailable or unusable ({err:#})");
            return Ok(None);
        }
    };
    tmux.send_keys(&["C-l"])?;

    tmux.send_text(&format!(
        "export HARNX_CONFIG_DIR={}",
        shell_escape(paths.harnx_config_dir.to_string_lossy().as_ref())
    ))?;
    tmux.send_keys(&["Enter"])?;

    tmux.send_text(&format!(
        "export PATH={} && cd {}; printf '__HANDOFF_READY__\\n'",
        shell_escape(&path_env),
        shell_escape(repo_root.to_string_lossy().as_ref())
    ))?;
    tmux.send_keys(&["Enter"])?;
    tmux.wait_for(Duration::from_secs(5), |screen| {
        count_occurrences(screen, "__HANDOFF_READY__") >= 2
    })?;

    Ok(Some(HandoffTmuxSession {
        _temp: temp,
        mock,
        tmux,
        harnx_bin,
    }))
}

fn handoff_script() -> MockOpenAiScript {
    MockOpenAiScript {
        turns: vec![
            MockOpenAiTurn {
                text_chunks: vec!["I'll hand this off to the specialist.".to_string()],
                tool_calls: vec![MockOpenAiToolCall {
                    name: format!("{}_session_handoff", HANDOFF_SUB_AGENT_NAME),
                    arguments: json!({
                        "prompt": "Take over and answer how many files are in /tmp.",
                        "session_id": HANDOFF_SESSION_ID,
                    }),
                    id: Some("call_handoff_1".to_string()),
                }],
                error: None,
            },
            MockOpenAiTurn {
                text_chunks: vec![HANDOFF_FINAL_RESPONSE.to_string()],
                tool_calls: vec![],
                error: None,
            },
            MockOpenAiTurn {
                text_chunks: vec![HANDOFF_REUSE_FOLLOWUP_RESPONSE.to_string()],
                tool_calls: vec![],
                error: None,
            },
        ],
        fallback_text: "No more scripted responses.".to_string(),
        chunk_delay_ms: 0,
    }
}

fn write_handoff_fixture_files(paths: &TestPaths) -> Result<()> {
    std::fs::create_dir_all(&paths.harnx_config_dir)?;

    let harnx_bin = repo_root()?
        .join("target")
        .join("debug")
        .join(binary_name("harnx"));

    std::fs::write(
        &paths.config_path,
        "save: false
",
    )?;

    let clients_dir = paths.harnx_config_dir.join("clients");
    let acp_servers_dir = paths.harnx_config_dir.join("acp_servers");
    std::fs::create_dir_all(&clients_dir)?;
    std::fs::create_dir_all(&acp_servers_dir)?;
    let client = serde_yaml::Value::Mapping(serde_yaml::Mapping::from_iter([
        (
            serde_yaml::Value::String("type".to_string()),
            serde_yaml::Value::String("openai-compatible".to_string()),
        ),
        (
            serde_yaml::Value::String("name".to_string()),
            serde_yaml::Value::String("mock-llm".to_string()),
        ),
        (
            serde_yaml::Value::String("api_base".to_string()),
            serde_yaml::Value::String(format!("http://127.0.0.1:{}/v1", paths.port)),
        ),
        (
            serde_yaml::Value::String("api_key".to_string()),
            serde_yaml::Value::String("dummy".to_string()),
        ),
        (
            serde_yaml::Value::String("models".to_string()),
            serde_yaml::Value::Sequence(vec![serde_yaml::Value::Mapping(
                serde_yaml::Mapping::from_iter([
                    (
                        serde_yaml::Value::String("name".to_string()),
                        serde_yaml::Value::String("test".to_string()),
                    ),
                    (
                        serde_yaml::Value::String("max_input_tokens".to_string()),
                        serde_yaml::Value::Number(32000.into()),
                    ),
                    (
                        serde_yaml::Value::String("max_output_tokens".to_string()),
                        serde_yaml::Value::Number(4096.into()),
                    ),
                    (
                        serde_yaml::Value::String("supports_tool_use".to_string()),
                        serde_yaml::Value::Bool(true),
                    ),
                ]),
            )]),
        ),
    ]));
    std::fs::write(
        clients_dir.join("mock-llm.yaml"),
        serde_yaml::to_string(&client)?,
    )?;

    let acp_server = AcpServerConfig {
        name: HANDOFF_SUB_AGENT_NAME.to_string(),
        command: harnx_bin.to_string_lossy().into_owned(),
        args: vec!["--acp".to_string(), HANDOFF_SUB_AGENT_NAME.to_string()],
        env: Default::default(),
        enabled: true,
        description: None,
        idle_timeout_secs: 300,
        operation_timeout_secs: 3600,
    };
    std::fs::write(
        acp_servers_dir.join(format!("{}.yaml", HANDOFF_SUB_AGENT_NAME)),
        serde_yaml::to_string(&acp_server)?,
    )?;
    std::fs::write(
        paths.agents_dir.join(format!("{}.md", HANDOFF_AGENT_NAME)),
        format!(
            "---\nname: {}\nmodel: mock-llm:test\nuse_tools: {}_session_handoff\n---\nYou are {}. Hand off the task to {} using the handoff tool.\n",
            HANDOFF_AGENT_NAME,
            HANDOFF_SUB_AGENT_NAME,
            HANDOFF_AGENT_NAME,
            HANDOFF_SUB_AGENT_NAME,
        ),
    )?;
    std::fs::write(
        paths.agents_dir.join(format!("{}.md", HANDOFF_SUB_AGENT_NAME)),
        format!(
            "---\nname: {}\nmodel: mock-llm:test\n---\nYou are {}. Take over the interactive session and answer directly.\n",
            HANDOFF_SUB_AGENT_NAME,
            HANDOFF_SUB_AGENT_NAME,
        ),
    )?;
    Ok(())
}
