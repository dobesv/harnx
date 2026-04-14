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
    // A real tool call row looks like "→ repro249_unique_mcp_tool"
    screen.contains(&format!("→ {}", REPRO_249_MCP_TOOL_NAME))
}

fn normalize_screen(screen: &str) -> String {
    screen
        .lines()
        .map(|line| line.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Replace spinner glyphs (animated braille chars and the idle "•" bullet)
/// with a fixed placeholder so snapshots are deterministic across runs.
fn normalize_spinner_chars(text: &str) -> String {
    const SPINNER_CHARS: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏', '•'];
    text.chars()
        .map(|c| if SPINNER_CHARS.contains(&c) { '*' } else { c })
        .collect()
}

/// Replace any UUIDv4-looking substring with a placeholder so snapshots are
/// deterministic across runs.
fn normalize_uuids(text: &str) -> String {
    // UUID pattern: 8-4-4-4-12 hex chars separated by hyphens.
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if is_uuid_at(&chars, i) {
            out.push_str("<UUID>");
            i += 36;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

fn is_uuid_at(chars: &[char], i: usize) -> bool {
    if i + 36 > chars.len() {
        return false;
    }
    // Group lengths 8-4-4-4-12 separated by hyphens at indices 8, 13, 18, 23.
    for (idx, &c) in chars[i..i + 36].iter().enumerate() {
        let is_hyphen_pos = matches!(idx, 8 | 13 | 18 | 23);
        if is_hyphen_pos {
            if c != '-' {
                return false;
            }
        } else if !c.is_ascii_hexdigit() {
            return false;
        }
    }
    // Don't match UUIDs preceded by another hex digit or hyphen (to avoid
    // mid-string matches within longer identifiers).
    if i > 0 {
        let prev = chars[i - 1];
        if prev.is_ascii_hexdigit() || prev == '-' {
            return false;
        }
    }
    true
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

// ── Nested sub-agent duplication test ────────────────────────────────────────
//
// Parent → sub-agent (researcher) → sub-sub-agent (analyst).
// Each level streams text, makes tool calls, and delegates further.
// The test captures the final screen and asserts that no activity is duplicated.

const NESTED_PARENT_AGENT: &str = "nested-parent";
const NESTED_SUB_AGENT: &str = "nested-researcher";
const NESTED_SUB_SUB_AGENT: &str = "nested-analyst";

#[test]
fn nested_sub_agent_activity_no_duplicates() -> Result<()> {
    if option_env!("CARGO_BIN_NAME") == Some("harnx") {
        eprintln!("skipping nested_sub_agent_activity_no_duplicates in binary test target");
        return Ok(());
    }
    if !TmuxHarness::is_available() {
        eprintln!("skipping nested_sub_agent_activity_no_duplicates: tmux is unavailable");
        return Ok(());
    }

    let repo_root = repo_root()?;
    let target_dir = repo_root.join("target").join("debug");
    let harnx_bin = target_dir.join(binary_name("harnx"));
    if !harnx_bin.is_file() {
        eprintln!("skipping nested_sub_agent_activity_no_duplicates: harnx binary is missing");
        return Ok(());
    }
    let harnx_mcp_bin = target_dir.join(binary_name("harnx-mcp-repro249"));
    if !harnx_mcp_bin.is_file() {
        eprintln!(
            "skipping nested_sub_agent_activity_no_duplicates: harnx-mcp-repro249 binary missing"
        );
        return Ok(());
    }

    let temp = TempDir::new().context("failed to create temp dir")?;
    let mock = MockOpenAiServer::start(nested_script())?;
    let paths = TestPaths::new(temp.path(), mock.port())?;
    write_nested_fixture_files(&paths)?;

    let path_env = format!(
        "{}:{}",
        target_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let tmux = match TmuxHarness::new(&repo_root, 120, 80) {
        Ok(tmux) => tmux,
        Err(err) => {
            eprintln!("skipping nested_sub_agent_activity_no_duplicates: tmux unusable ({err:#})");
            return Ok(());
        }
    };
    tmux.send_keys(&["C-l"])?;

    // Export HARNX_CONFIG_DIR
    tmux.send_text(&format!(
        "export HARNX_CONFIG_DIR={}",
        shell_escape(paths.harnx_config_dir.to_string_lossy().as_ref())
    ))?;
    tmux.send_keys(&["Enter"])?;

    // Export PATH
    tmux.send_text(&format!(
        "export PATH={} && cd {}; printf '__NESTED_READY__\\n'",
        shell_escape(&path_env),
        shell_escape(repo_root.to_string_lossy().as_ref())
    ))?;
    tmux.send_keys(&["Enter"])?;
    tmux.wait_for(Duration::from_secs(5), |screen| {
        count_occurrences(screen, "__NESTED_READY__") >= 2
    })?;

    // Launch the parent agent
    tmux.send_text(&format!(
        "{} -a {} || echo HARNX_EXIT:$?",
        shell_escape(harnx_bin.to_string_lossy().as_ref()),
        NESTED_PARENT_AGENT
    ))?;
    tmux.send_keys(&["Enter"])?;

    tmux.wait_for_contains("Welcome to harnx", Duration::from_secs(15))?;
    tmux.send_text("investigate the data trends")?;
    tmux.send_keys(&["Enter"])?;

    // Wait for delegation chain to complete: parent delegates to researcher,
    // researcher delegates to analyst, analyst calls MCP tool, then everything
    // unwinds with final messages at each level.
    //
    // Wait for the *full* parent final message so the spinner has settled and
    // the text has finished streaming (not just the "PARENT_FINAL:" marker,
    // which can appear before the rest of the message arrives).
    let screen = tmux.wait_for(Duration::from_secs(45), |screen| {
        screen.contains("PARENT_FINAL: Research complete. Data shows clear upward trend.")
    })?;

    // ── Verify the delegation chain is visible ───────────────────────────────
    assert!(
        screen.contains(&format!("{}_session_prompt", NESTED_SUB_AGENT)),
        "parent→researcher delegation tool call not visible:\n{screen}"
    );
    assert!(
        screen.contains(&format!("> {}", NESTED_SUB_AGENT)),
        "researcher sub-agent heading not visible:\n{screen}"
    );
    assert!(
        screen.contains(&format!("{}_session_prompt", NESTED_SUB_SUB_AGENT)),
        "researcher→analyst delegation tool call not visible:\n{screen}"
    );
    assert!(
        screen.contains(&format!("> {}", NESTED_SUB_SUB_AGENT)),
        "analyst sub-sub-agent heading not visible:\n{screen}"
    );
    assert!(
        screen.contains(REPRO_249_MCP_TOOL_NAME),
        "analyst's MCP tool call not visible:\n{screen}"
    );

    // Print the full screen for debugging when the test fails.
    eprintln!(
        "\n=== nested sub-agent duplication test screen ===\n{}\n====\n",
        screen
    );

    let normalized = normalize_spinner_chars(&normalize_uuids(&normalize_screen(&screen)));
    assert_snapshot!("nested_sub_agent_activity_no_duplicates", normalized);

    // ── Duplication assertions ───────────────────────────────────────────────
    // Each delegation tool call should appear exactly once.
    assert_eq!(
        count_occurrences(&screen, &format!("{}_session_prompt", NESTED_SUB_AGENT)),
        1,
        "researcher delegation tool call appears more than once:\n{screen}"
    );
    assert_eq!(
        count_occurrences(&screen, &format!("{}_session_prompt", NESTED_SUB_SUB_AGENT)),
        1,
        "analyst delegation tool call appears more than once:\n{screen}"
    );

    // The MCP tool call from the analyst should appear exactly once.
    // (The researcher also calls it, so we expect exactly 2 total.)
    let mcp_tool_occurrences = count_occurrences(&screen, REPRO_249_MCP_TOOL_NAME);
    assert_eq!(
        mcp_tool_occurrences, 2,
        "MCP tool call should appear exactly twice (once per agent that calls it), got {mcp_tool_occurrences}:\n{screen}"
    );

    // The researcher heading may appear 1 OR 2 times: once when parent first
    // delegates to it, and optionally once more when control returns from
    // analyst (a source-transition event like Usage re-asserts the researcher
    // context). Both counts are semantically valid — the assertion here just
    // guards against runaway duplication (e.g., a heading per streamed chunk).
    let researcher_headings = screen
        .lines()
        .filter(|line| {
            line.contains(&format!("> {}", NESTED_SUB_AGENT))
                && !line.contains(&format!("> {}", NESTED_SUB_SUB_AGENT))
                && !line.contains("in ")
                && !line.contains("out ")
        })
        .count();
    assert!(
        (1..=2).contains(&researcher_headings),
        "researcher heading should appear 1 or 2 times, got {researcher_headings}:\n{screen}"
    );
    let analyst_headings = screen
        .lines()
        .filter(|line| {
            line.contains(&format!("> {}", NESTED_SUB_SUB_AGENT))
                && !line.contains("in ")
                && !line.contains("out ")
        })
        .count();
    assert_eq!(
        analyst_headings, 1,
        "analyst heading should appear exactly once (excluding usage lines), got {analyst_headings}:\n{screen}"
    );

    // Final message markers are expected to appear in the live stream AND
    // possibly in the tool-call result display (since `session_prompt`
    // legitimately returns the accumulated response text).  The assertion
    // guards against runaway duplication (e.g., each chunk emitted twice).
    //
    // RESEARCHER_TEXT and ANALYST_TEXT may appear up to 2 times:
    //   - once in the live stream from the sub-agent
    //   - once more in the tool-call result rendering
    // PARENT_FINAL only comes from the parent's final LLM turn (never wrapped
    // in a tool result), so it should appear exactly once.
    for marker in ["RESEARCHER_TEXT:", "ANALYST_TEXT:"] {
        let marker_count = count_occurrences(&screen, marker);
        assert!(
            (1..=2).contains(&marker_count),
            "{marker} should appear 1 or 2 times (live stream + optional tool result), got {marker_count}:\n{screen}"
        );
    }
    let parent_final_count = count_occurrences(&screen, "PARENT_FINAL:");
    assert_eq!(
        parent_final_count, 1,
        "PARENT_FINAL: should appear exactly once, got {parent_final_count}:\n{screen}"
    );

    drop(tmux);
    drop(mock);
    Ok(())
}

/// Mock LLM script for the nested delegation test.
///
/// Turns are consumed in order across all three processes:
///   Turn 0 (parent)         : stream text + delegate to researcher
///   Turn 1 (researcher)     : stream text + call MCP tool (to have a tool call before delegating)
///   Turn 2 (researcher)     : stream text + delegate to analyst
///   Turn 3 (analyst)        : stream text + call MCP tool
///   Turn 4 (analyst)        : stream final text (ends analyst)
///   Turn 5 (researcher)     : stream final text (ends researcher)
///   Turn 6 (parent)         : stream final text (ends parent turn)
fn nested_script() -> MockOpenAiScript {
    MockOpenAiScript {
        turns: vec![
            // Turn 0: parent streams opening + delegates to researcher
            MockOpenAiTurn {
                text_chunks: vec![
                    "Let me ".to_string(),
                    "investigate ".to_string(),
                    "this. ".to_string(),
                    "Delegating to researcher.".to_string(),
                ],
                tool_calls: vec![MockOpenAiToolCall {
                    name: format!("{}_session_prompt", NESTED_SUB_AGENT),
                    arguments: json!({
                        "message": "Investigate the data trends in detail."
                    }),
                    id: Some("call_delegate_researcher".to_string()),
                }],
                error: None,
            },
            // Turn 1: researcher streams text + calls MCP tool
            MockOpenAiTurn {
                text_chunks: vec![
                    "RESEARCHER_TEXT: ".to_string(),
                    "Let me check ".to_string(),
                    "the raw data first.".to_string(),
                ],
                tool_calls: vec![MockOpenAiToolCall {
                    name: REPRO_249_MCP_TOOL_NAME.to_string(),
                    arguments: json!({}),
                    id: Some("call_researcher_mcp".to_string()),
                }],
                error: None,
            },
            // Turn 2: researcher streams text + delegates to analyst
            MockOpenAiTurn {
                text_chunks: vec![
                    "Got initial data. ".to_string(),
                    "Need deeper analysis. ".to_string(),
                    "Delegating to analyst.".to_string(),
                ],
                tool_calls: vec![MockOpenAiToolCall {
                    name: format!("{}_session_prompt", NESTED_SUB_SUB_AGENT),
                    arguments: json!({
                        "message": "Perform deep analysis on the data trends."
                    }),
                    id: Some("call_delegate_analyst".to_string()),
                }],
                error: None,
            },
            // Turn 3: analyst streams text + calls MCP tool
            MockOpenAiTurn {
                text_chunks: vec![
                    "ANALYST_TEXT: ".to_string(),
                    "Running deep ".to_string(),
                    "analysis now.".to_string(),
                ],
                tool_calls: vec![MockOpenAiToolCall {
                    name: REPRO_249_MCP_TOOL_NAME.to_string(),
                    arguments: json!({}),
                    id: Some("call_analyst_mcp".to_string()),
                }],
                error: None,
            },
            // Turn 4: analyst final text (ends analyst)
            MockOpenAiTurn {
                text_chunks: vec![
                    "Analysis complete. ".to_string(),
                    "The trend is clearly upward.".to_string(),
                ],
                tool_calls: vec![],
                error: None,
            },
            // Turn 5: researcher final text (ends researcher)
            MockOpenAiTurn {
                text_chunks: vec![
                    "Analyst confirmed ".to_string(),
                    "the upward trend in the data.".to_string(),
                ],
                tool_calls: vec![],
                error: None,
            },
            // Turn 6: parent final text
            MockOpenAiTurn {
                text_chunks: vec![
                    "PARENT_FINAL: ".to_string(),
                    "Research complete. ".to_string(),
                    "Data shows clear ".to_string(),
                    "upward trend.".to_string(),
                ],
                tool_calls: vec![],
                error: None,
            },
        ],
        fallback_text: "No more scripted responses.".to_string(),
        chunk_delay_ms: 10,
    }
}

fn write_nested_fixture_files(paths: &TestPaths) -> Result<()> {
    std::fs::create_dir_all(&paths.harnx_config_dir)?;

    let fake_mcp_server = repo_root()?
        .join("target")
        .join("debug")
        .join(binary_name("harnx-mcp-repro249"));

    let harnx_bin = repo_root()?
        .join("target")
        .join("debug")
        .join(binary_name("harnx"));

    std::fs::write(&paths.config_path, "save: false\n")?;

    let clients_dir = paths.harnx_config_dir.join("clients");
    let mcp_servers_dir = paths.harnx_config_dir.join("mcp_servers");
    let acp_servers_dir = paths.harnx_config_dir.join("acp_servers");
    std::fs::create_dir_all(&clients_dir)?;
    std::fs::create_dir_all(&mcp_servers_dir)?;
    std::fs::create_dir_all(&acp_servers_dir)?;

    // Client config (shared by all agents via the mock LLM server)
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

    // MCP server (used by both researcher and analyst)
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

    // ACP server: researcher (sub-agent)
    let researcher_acp = AcpServerConfig {
        name: NESTED_SUB_AGENT.to_string(),
        command: harnx_bin.to_string_lossy().into_owned(),
        args: vec!["--acp".to_string(), NESTED_SUB_AGENT.to_string()],
        env: Default::default(),
        enabled: true,
        description: None,
        idle_timeout_secs: 300,
        operation_timeout_secs: 3600,
    };
    std::fs::write(
        acp_servers_dir.join(format!("{}.yaml", NESTED_SUB_AGENT)),
        serde_yaml::to_string(&researcher_acp)?,
    )?;

    // ACP server: analyst (sub-sub-agent)
    let analyst_acp = AcpServerConfig {
        name: NESTED_SUB_SUB_AGENT.to_string(),
        command: harnx_bin.to_string_lossy().into_owned(),
        args: vec!["--acp".to_string(), NESTED_SUB_SUB_AGENT.to_string()],
        env: Default::default(),
        enabled: true,
        description: None,
        idle_timeout_secs: 300,
        operation_timeout_secs: 3600,
    };
    std::fs::write(
        acp_servers_dir.join(format!("{}.yaml", NESTED_SUB_SUB_AGENT)),
        serde_yaml::to_string(&analyst_acp)?,
    )?;

    // Agent definitions
    // Parent: can delegate to researcher
    std::fs::write(
        paths.agents_dir.join(format!("{}.md", NESTED_PARENT_AGENT)),
        format!(
            "---\nname: {}\nmodel: mock-llm:test\nuse_tools: {}_session_prompt\n---\nYou are {}. Delegate tasks to {} and summarize.\n",
            NESTED_PARENT_AGENT,
            NESTED_SUB_AGENT,
            NESTED_PARENT_AGENT,
            NESTED_SUB_AGENT,
        ),
    )?;
    // Researcher: can call MCP tool and delegate to analyst
    std::fs::write(
        paths
            .agents_dir
            .join(format!("{}.md", NESTED_SUB_AGENT)),
        format!(
            "---\nname: {}\nmodel: mock-llm:test\nuse_tools: {}, {}_session_prompt\n---\nYou are {}. Use {} for data and delegate deep analysis to {}.\n",
            NESTED_SUB_AGENT,
            REPRO_249_MCP_TOOL_NAME,
            NESTED_SUB_SUB_AGENT,
            NESTED_SUB_AGENT,
            REPRO_249_MCP_TOOL_NAME,
            NESTED_SUB_SUB_AGENT,
        ),
    )?;
    // Analyst: can call MCP tool
    std::fs::write(
        paths
            .agents_dir
            .join(format!("{}.md", NESTED_SUB_SUB_AGENT)),
        format!(
            "---\nname: {}\nmodel: mock-llm:test\nuse_tools: {}\n---\nYou are {}. Use {} to perform deep analysis.\n",
            NESTED_SUB_SUB_AGENT,
            REPRO_249_MCP_TOOL_NAME,
            NESTED_SUB_SUB_AGENT,
            REPRO_249_MCP_TOOL_NAME,
        ),
    )?;

    Ok(())
}
