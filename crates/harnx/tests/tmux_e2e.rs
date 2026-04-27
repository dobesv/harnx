use harnx::mcp::McpServerConfig;
use harnx::test_utils::{
    harnx_mcp_repro249_bin, MockOpenAiError, MockOpenAiScript, MockOpenAiServer,
    MockOpenAiToolCall, MockOpenAiTurn, TmuxHarness,
};
use harnx_acp::AcpServerConfig;

use anyhow::{Context, Result};
use insta::assert_snapshot;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::TempDir;

const REPRO_249_MCP_TOOL_NAME: &str = "repro249_unique_mcp_tool";
const REPRO_249_MCP_TOOL_RESPONSE: &str = "repro249 fixed tool response";
const TEST_AGENT_NAME: &str = "test-agent";
const TEST_SUB_AGENT_NAME: &str = "test-sub-agent";

#[test]
fn repro_249_top_level_delegation_markers() -> Result<()> {
    if !TmuxHarness::is_available() {
        eprintln!("skipping repro_249_top_level_delegation_markers: tmux is unavailable");
        return Ok(());
    }

    let repo_root = repo_root()?;
    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));

    let temp = TempDir::new().context("failed to create temp dir")?;
    let mock = MockOpenAiServer::start(script())?;
    let paths = TestPaths::new(temp.path(), mock.port())?;
    write_fixture_files(&paths)?;

    let path_env = format!(
        "{}:{}",
        harnx_bin
            .parent()
            .context("harnx binary missing parent directory")?
            .display(),
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
    let agents_screen = tmux.wait_for(Duration::from_secs(10), |screen| {
        count_occurrences(screen, "__READY_3__") >= 2
    })?;
    assert!(
        agents_screen.contains(TEST_AGENT_NAME),
        "main agent not listed in --list-agents output:\n{agents_screen}"
    );
    assert!(
        agents_screen.contains(TEST_SUB_AGENT_NAME),
        "sub-agent not listed in --list-agents output:\n{agents_screen}"
    );

    let delegate_tool_name = format!("{}_session_prompt", TEST_SUB_AGENT_NAME);

    // Step 5: Start interactive TUI session
    tmux.send_text(&format!(
        "{} -a {} || echo HARNX_EXIT:$?",
        shell_escape(harnx_bin.to_string_lossy().as_ref()),
        TEST_AGENT_NAME
    ))?;
    tmux.send_keys(&["Enter"])?;

    tmux.wait_for_contains("Welcome to harnx", Duration::from_secs(15))?;
    tmux.send_text("call repro249 through the sub-agent")?;
    tmux.send_keys(&["Enter"])?;

    let screen = tmux.wait_for(Duration::from_secs(30), |screen| {
        screen.contains(REPRO_249_MCP_TOOL_RESPONSE)
    })?;

    assert_eq!(
        count_occurrences(&screen, &format!("→ {delegate_tool_name}")),
        1,
        "expected exactly one top-level delegation marker, got screen:\n{screen}"
    );
    assert!(
        count_occurrences(&screen, REPRO_249_MCP_TOOL_NAME) >= 1,
        "expected MCP tool marker to appear at least once, got screen:\n{screen}"
    );
    // The response phrase appears in three legitimate places once the
    // delegated session completes end-to-end: the sub-agent's streamed
    // text, the parent's `Tool::Completed` rendering of the
    // `_session_prompt` result (which echoes the sub-agent's accumulated
    // output), and the parent's own final reply that references it.
    // None of those are duplicates of the same render — they are three
    // distinct events whose payload happens to share a substring. The
    // earlier `count == 1` assertion was an artifact of OLD-code
    // behavior where the sub-agent's MCP tool errored out (so the
    // response phrase only appeared in the sub-agent's mock text). The
    // function-name assertion above already verifies the actual #249
    // invariant: exactly one top-level delegation marker.
    assert!(
        count_occurrences(&screen, REPRO_249_MCP_TOOL_RESPONSE) >= 1,
        "expected MCP tool response to appear at least once, got screen:\n{screen}"
    );
    assert!(
        !screen.contains("HARNX_EXIT:"),
        "harnx exited unexpectedly:\n{screen}"
    );

    drop(tmux);
    drop(mock);
    Ok(())
}

// Normalizes screen output for snapshot tests.
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
    let mut i = 0usize;
    while i < chars.len() {
        if is_uuid_at(&chars, i) {
            out.push_str("[UUID]");
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
    // Don't match UUIDs followed by another hex digit or hyphen.
    if i + 36 < chars.len() {
        let next = chars[i + 36];
        if next.is_ascii_hexdigit() || next == '-' {
            return false;
        }
    }
    true
}

fn shell_escape(s: &str) -> String {
    let escaped = s.replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn count_occurrences(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

fn repo_root() -> Result<PathBuf> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .context("failed to determine workspace root")
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
                text_chunks: vec!["I'll use the MCP tool now.".to_string()],
                tool_calls: vec![MockOpenAiToolCall {
                    name: REPRO_249_MCP_TOOL_NAME.to_string(),
                    arguments: json!({}),
                    id: Some("call_mcp_1".to_string()),
                }],
                error: None,
            },
            // Turn 2: sub-agent summarizes and returns
            MockOpenAiTurn {
                text_chunks: vec![format!(
                    "Sub-agent saw MCP result: {REPRO_249_MCP_TOOL_RESPONSE}"
                )],
                tool_calls: vec![],
                error: None,
            },
            // Turn 3: parent final reply after delegated task completes
            MockOpenAiTurn {
                text_chunks: vec![format!(
                    "Done. Delegated task completed with: {REPRO_249_MCP_TOOL_RESPONSE}"
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

    std::fs::write(&paths.config_path, "save: false\n")?;

    let clients_dir = paths.harnx_config_dir.join("clients");
    let mcp_servers_dir = paths.harnx_config_dir.join("mcp_servers");
    std::fs::create_dir_all(&clients_dir)?;
    std::fs::create_dir_all(&mcp_servers_dir)?;

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

    let repro249_bin = harnx_mcp_repro249_bin(&PathBuf::from(env!("CARGO_BIN_EXE_harnx")));
    let mcp_server = McpServerConfig {
        name: REPRO_249_MCP_TOOL_NAME.to_string(),
        command: repro249_bin.to_string_lossy().into_owned(),
        args: vec![],
        env: Default::default(),
        enabled: true,
        roots: vec![],
        description: None,
        rename_tools: Default::default(),
        tool_templates: Default::default(),
    };
    std::fs::write(
        mcp_servers_dir.join("repro249.yaml"),
        serde_yaml::to_string(&mcp_server)?,
    )?;

    std::fs::write(
        paths.agents_dir.join(format!("{}.md", TEST_AGENT_NAME)),
        format!(
            "---\nname: {}\nmodel: mock-llm:test\nuse_tools: {}_session_prompt\n---\nYou are {}. Delegate work to {}.\n",
            TEST_AGENT_NAME, TEST_SUB_AGENT_NAME, TEST_AGENT_NAME, TEST_SUB_AGENT_NAME
        ),
    )?;
    std::fs::write(
        paths.agents_dir.join(format!("{}.md", TEST_SUB_AGENT_NAME)),
        format!(
            "---\nname: {}\nmodel: mock-llm:test\nuse_tools: {}\n---\nYou are {}. Use the MCP tool and report the result.\n",
            TEST_SUB_AGENT_NAME,
            REPRO_249_MCP_TOOL_NAME,
            TEST_SUB_AGENT_NAME
        ),
    )?;
    Ok(())
}

// ── Planning → Execution handoff scenario ─────────────────────────────────────
//
// Scenario: Planner asks clarifying question → User answers → Planner creates
// plan → Planner asks if ready to hand off → User says yes → Session handoff to
// executor → Executor runs step 1 → Executor reports done → User gives feedback
// → Executor adjusts and runs step 2 → Executor reports final done.

const EXECUTOR_AGENT_NAME: &str = "plan-executor";
const HANDOFF_SESSION_ID: &str = "exec-session-1";

// Planner phase
const PLANNER_QUESTION: &str = "What feature would you like me to plan?";
const USER_ANSWER: &str = "Add a dark mode toggle";
const PLANNER_PLAN_CREATED: &str =
    "Plan created: 1) Add theme toggle component 2) Add CSS variables 3) Wire up state persistence";
const PLANNER_HANDOFF_ASK: &str = "Ready to hand off to plan-executor. Proceed?";
const USER_CONFIRM: &str = "yes";

// Executor phase 1 - initial execution
const EXECUTOR_STEP1_TOOL: &str = "create_file";
const EXECUTOR_STEP1_TEXT: &str = "Executing step 1: creating theme toggle component.";
const EXECUTOR_DONE_RESPONSE: &str = "Plan execution complete. Theme toggle is ready.";

// User feedback phase
const USER_FEEDBACK: &str = "Actually, can you also add a shortcut key?";

// Executor phase 2 - feedback incorporation
const EXECUTOR_STEP2_TOOL: &str = "edit_file";
const EXECUTOR_STEP2_TEXT: &str = "Adding keyboard shortcut for theme toggle.";
const EXECUTOR_FINAL_RESPONSE: &str = "Done! Theme toggle now has keyboard shortcut Ctrl+Shift+T.";

// Legacy constants for backward compatibility with other tests
const HANDOFF_AGENT_NAME: &str = "planner";
const HANDOFF_SUB_AGENT_NAME: &str = "plan-executor";
const HANDOFF_PROMPT: &str = "Execute the plan: add dark mode toggle with keyboard shortcut.";
const HANDOFF_ORIGINAL_USER_TEXT: &str = "plan a dark mode feature";
const HANDOFF_SUB_AGENT_SYSTEM_PROMPT: &str =
    "You are a plan executor. Execute the plan step by step using available tools.";
const HANDOFF_FINAL_RESPONSE: &str = EXECUTOR_FINAL_RESPONSE;
const HANDOFF_SUB_AGENT_TOOL_NAME: &str = EXECUTOR_STEP1_TOOL;

#[test]
fn interactive_handoff_planner_to_executor() -> Result<()> {
    let session = match setup_handoff_tmux_session("interactive_handoff_planner_to_executor")? {
        Some(session) => session,
        None => return Ok(()),
    };

    let HandoffTmuxSession {
        tmux,
        mock,
        harnx_bin,
        ..
    } = session;

    run_handoff_command(&tmux, &harnx_bin)?;

    // Turn 1: User gives initial request, planner asks clarifying question
    tmux.send_text(HANDOFF_ORIGINAL_USER_TEXT)?;
    tmux.send_keys(&["Enter"])?;
    let _first_screen = tmux.wait_for(Duration::from_secs(30), |screen| {
        screen.contains(PLANNER_QUESTION)
    })?;

    // Turn 2: User answers, planner creates plan and asks about handoff
    tmux.send_text(USER_ANSWER)?;
    tmux.send_keys(&["Enter"])?;
    let _second_screen = tmux.wait_for(Duration::from_secs(30), |screen| {
        screen.contains("Ready to hand off")
    })?;

    // Turn 3: User confirms, planner hands off to executor
    tmux.send_text(USER_CONFIRM)?;
    tmux.send_keys(&["Enter"])?;
    let _third_screen = tmux.wait_for(Duration::from_secs(30), |screen| {
        screen.contains(&format!("{}_session_handoff", EXECUTOR_AGENT_NAME))
    })?;

    // Wait for executor to start and run first tool
    let _fourth_screen = tmux.wait_for(Duration::from_secs(30), |screen| {
        screen.contains(EXECUTOR_STEP1_TOOL)
    })?;

    // Wait for executor to report done
    let _fifth_screen = tmux.wait_for(Duration::from_secs(30), |screen| {
        screen.contains(EXECUTOR_DONE_RESPONSE)
    })?;

    // Turn 4: User gives feedback
    tmux.send_text(USER_FEEDBACK)?;
    tmux.send_keys(&["Enter"])?;

    // Wait for executor to run second tool
    let _sixth_screen = tmux.wait_for(Duration::from_secs(30), |screen| {
        screen.contains(EXECUTOR_STEP2_TOOL)
    })?;

    // Wait for final response
    let final_screen = tmux.wait_for(Duration::from_secs(30), |screen| {
        screen.contains(EXECUTOR_FINAL_RESPONSE)
    })?;

    // Assert the key elements are visible
    assert!(
        final_screen.contains(PLANNER_PLAN_CREATED),
        "plan creation not visible: {final_screen}"
    );
    assert!(
        final_screen.contains(&format!("{}_session_handoff", EXECUTOR_AGENT_NAME)),
        "handoff tool call not visible: {final_screen}"
    );
    assert!(
        final_screen.contains(HANDOFF_SESSION_ID),
        "session id not visible: {final_screen}"
    );
    assert!(
        final_screen.contains(EXECUTOR_STEP1_TOOL),
        "executor first tool call not visible: {final_screen}"
    );
    assert!(
        final_screen.contains(EXECUTOR_STEP2_TOOL),
        "executor second tool call not visible: {final_screen}"
    );
    assert!(
        final_screen.contains(EXECUTOR_FINAL_RESPONSE),
        "final response not visible: {final_screen}"
    );

    let normalized = normalize_screen(&final_screen);
    assert_snapshot!("interactive_handoff_planner_to_executor", normalized);

    drop(tmux);
    drop(mock);
    Ok(())
}

#[test]
fn handoff_without_acp_server_config() -> Result<()> {
    let session =
        match setup_handoff_tmux_session_no_acp_server("handoff_without_acp_server_config")? {
            Some(session) => session,
            None => return Ok(()),
        };

    let HandoffTmuxSession {
        tmux,
        mock,
        harnx_bin,
        ..
    } = session;

    run_handoff_command(&tmux, &harnx_bin)?;
    send_handoff_user_prompt(&tmux)?;

    let screen = wait_for_handoff_completion(&tmux)?;
    assert_handoff_screen(&screen, "no-ACP transcript")?;

    drop(tmux);
    drop(mock);
    Ok(())
}

#[test]
fn handoff_session_isolation() -> Result<()> {
    let session = match setup_handoff_tmux_session_with_script(
        "handoff_session_isolation",
        simple_handoff_script(),
        None,
        true,
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

    run_handoff_command(&tmux, &harnx_bin)?;
    send_handoff_user_prompt(&tmux)?;

    let screen = wait_for_handoff_completion(&tmux)?;
    assert_handoff_screen(&screen, "session isolation transcript")?;

    drop(tmux);
    let request_log = mock.get_request_log();
    assert_handoff_request_isolation(&request_log)?;
    drop(mock);
    Ok(())
}

fn run_handoff_command(tmux: &TmuxHarness, harnx_bin: &Path) -> Result<()> {
    tmux.send_text(&format!(
        "{} -a {} || echo HARNX_EXIT:$?",
        shell_escape(harnx_bin.to_string_lossy().as_ref()),
        HANDOFF_AGENT_NAME
    ))?;
    tmux.send_keys(&["Enter"])?;
    tmux.wait_for_contains("Welcome to harnx", Duration::from_secs(15))?;
    Ok(())
}

fn send_handoff_user_prompt(tmux: &TmuxHarness) -> Result<()> {
    tmux.send_text(HANDOFF_ORIGINAL_USER_TEXT)?;
    tmux.send_keys(&["Enter"])?;
    Ok(())
}

fn wait_for_handoff_completion(tmux: &TmuxHarness) -> Result<String> {
    tmux.wait_for(Duration::from_secs(30), |screen| {
        screen.contains(&format!("{}_session_handoff", HANDOFF_SUB_AGENT_NAME))
            && screen.contains(HANDOFF_FINAL_RESPONSE)
    })
}

fn assert_handoff_screen(screen: &str, label: &str) -> Result<()> {
    assert!(
        screen.contains(&format!("{}_session_handoff", HANDOFF_SUB_AGENT_NAME)),
        "handoff tool call not visible anywhere in {label}:\n{screen}"
    );
    assert!(
        screen.contains(HANDOFF_SESSION_ID),
        "handed-off session id not visible anywhere in {label}:\n{screen}"
    );
    assert!(
        screen.contains(HANDOFF_SUB_AGENT_TOOL_NAME),
        "sub-agent tool call '{HANDOFF_SUB_AGENT_TOOL_NAME}' not visible anywhere in {label}:\n{screen}"
    );
    assert!(
        screen.contains(HANDOFF_FINAL_RESPONSE),
        "final handed-off response not visible anywhere in {label}:\n{screen}"
    );
    Ok(())
}

fn request_messages<'a>(request: &'a Value, label: &str) -> Result<&'a Vec<Value>> {
    request
        .get("messages")
        .and_then(Value::as_array)
        .with_context(|| format!("{label} missing messages array: {request}"))
}

fn request_message_content(message: &Value, label: &str) -> Result<String> {
    match message.get("content") {
        Some(Value::String(content)) => Ok(content.clone()),
        Some(Value::Array(parts)) => Ok(parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<String>()),
        _ => anyhow::bail!("{label} missing string content: {message}"),
    }
}

fn assert_handoff_request_isolation(request_log: &[Value]) -> Result<()> {
    assert!(
        request_log.len() >= 2,
        "expected at least parent and handed-off LLM requests: {request_log:?}"
    );

    let parent_request = request_log
        .first()
        .with_context(|| format!("missing first LLM request in log: {request_log:?}"))?;
    let parent_messages = request_messages(parent_request, "first LLM request")?;
    assert_eq!(
        parent_messages.len(),
        2,
        "expected first LLM request to contain only parent system and original user messages: {parent_messages:?}"
    );
    assert_eq!(
        parent_messages[0].get("role").and_then(Value::as_str),
        Some("system"),
        "expected first message in first LLM request to be system: {}",
        parent_messages[0]
    );
    assert_eq!(
        parent_messages[1].get("role").and_then(Value::as_str),
        Some("user"),
        "expected second message in first LLM request to be user: {}",
        parent_messages[1]
    );
    let parent_system =
        request_message_content(&parent_messages[0], "first LLM request system message")?;
    let parent_user =
        request_message_content(&parent_messages[1], "first LLM request user message")?;
    assert!(
        parent_system.contains(HANDOFF_AGENT_NAME),
        "parent agent system prompt missing from first LLM request: {parent_system}"
    );
    assert_eq!(
        parent_user, HANDOFF_ORIGINAL_USER_TEXT,
        "original user text missing from first LLM request: {parent_user}"
    );

    let handoff_request = request_log
        .get(1)
        .with_context(|| format!("missing second LLM request in log: {request_log:?}"))?;
    let handoff_messages = request_messages(handoff_request, "second LLM request")?;
    assert_eq!(
        handoff_messages.len(),
        2,
        "expected second LLM request to contain only system and user messages for handed-off agent: {handoff_messages:?}"
    );

    let handoff_system =
        request_message_content(&handoff_messages[0], "second LLM request system message")?;
    let handoff_user =
        request_message_content(&handoff_messages[1], "second LLM request user message")?;
    assert_eq!(
        handoff_messages[0].get("role").and_then(Value::as_str),
        Some("system"),
        "expected first message in second LLM request to be system: {}",
        handoff_messages[0]
    );
    assert_eq!(
        handoff_messages[1].get("role").and_then(Value::as_str),
        Some("user"),
        "expected second message in second LLM request to be user: {}",
        handoff_messages[1]
    );
    assert!(
        handoff_system.contains(HANDOFF_SUB_AGENT_SYSTEM_PROMPT),
        "sub-agent system prompt missing from second LLM request: {handoff_system}"
    );
    assert_eq!(
        handoff_user, HANDOFF_PROMPT,
        "handoff prompt mismatch in second LLM request: {handoff_user}"
    );
    assert!(
        !handoff_system.contains(HANDOFF_ORIGINAL_USER_TEXT)
            && !handoff_user.contains(HANDOFF_ORIGINAL_USER_TEXT),
        "original parent user text leaked into second LLM request: {handoff_system}
{handoff_user}"
    );
    assert!(
        !handoff_system.contains("You are handoff-agent")
            && !handoff_user.contains("You are handoff-agent"),
        "parent agent system prompt leaked into second LLM request: {handoff_system}
{handoff_user}"
    );
    Ok(())
}

struct HandoffTmuxSession {
    _temp: TempDir,
    mock: MockOpenAiServer,
    tmux: TmuxHarness,
    harnx_bin: PathBuf,
}

fn setup_handoff_tmux_session(test_name: &str) -> Result<Option<HandoffTmuxSession>> {
    setup_handoff_tmux_session_with_script(test_name, handoff_script(), None, true)
}

fn setup_handoff_tmux_session_no_acp_server(test_name: &str) -> Result<Option<HandoffTmuxSession>> {
    setup_handoff_tmux_session_with_script(
        test_name,
        simple_handoff_script(),
        Some(&format!("{}_session_handoff", HANDOFF_SUB_AGENT_NAME)),
        false,
    )
}

fn setup_handoff_tmux_session_with_script(
    test_name: &str,
    script: MockOpenAiScript,
    parent_use_tools: Option<&str>,
    write_acp_server: bool,
) -> Result<Option<HandoffTmuxSession>> {
    if option_env!("CARGO_BIN_NAME") == Some("harnx") {
        eprintln!("skipping {test_name} in binary test target to avoid duplicate tmux sessions");
        return Ok(None);
    }

    if !TmuxHarness::is_available() {
        eprintln!("skipping {test_name}: tmux is unavailable");
        return Ok(None);
    }

    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));

    let temp = TempDir::new().context("failed to create temp dir")?;
    let mock = MockOpenAiServer::start(script)?;
    let paths = TestPaths::new(temp.path(), mock.port())?;
    match (parent_use_tools, write_acp_server) {
        (Some(parent_use_tools), true) => {
            write_handoff_fixture_files_with_parent_use_tools(&paths, parent_use_tools)?;
        }
        (Some(parent_use_tools), false) => {
            write_handoff_fixture_files_no_acp_server_with_parent_use_tools(
                &paths,
                parent_use_tools,
            )?;
        }
        (None, true) => write_handoff_fixture_files(&paths)?,
        (None, false) => write_handoff_fixture_files_no_acp_server(&paths)?,
    }

    let path_env = format!(
        "{}:{}",
        harnx_bin
            .parent()
            .context("harnx binary missing parent directory")?
            .display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let root = repo_root()?;
    let tmux = match TmuxHarness::new(&root, 120, 35) {
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
        shell_escape(root.to_string_lossy().as_ref())
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
            // Turn 1: Planner asks clarifying question
            MockOpenAiTurn {
                text_chunks: vec![PLANNER_QUESTION.to_string()],
                tool_calls: vec![],
                error: None,
            },
            // Turn 2: Planner receives answer, creates plan, asks about handoff
            MockOpenAiTurn {
                text_chunks: vec![format!("{}. {}", PLANNER_PLAN_CREATED, PLANNER_HANDOFF_ASK)],
                tool_calls: vec![],
                error: None,
            },
            // Turn 3: Planner receives confirmation, hands off to executor
            MockOpenAiTurn {
                text_chunks: vec!["Great! Handing off to plan-executor.".to_string()],
                tool_calls: vec![MockOpenAiToolCall {
                    name: format!("{}_session_handoff", EXECUTOR_AGENT_NAME),
                    arguments: json!({
                        "prompt": HANDOFF_PROMPT,
                        "session_id": HANDOFF_SESSION_ID,
                    }),
                    id: Some("call_handoff_1".to_string()),
                }],
                error: None,
            },
            // Turn 4: Executor runs step 1 (create_file tool)
            MockOpenAiTurn {
                text_chunks: vec![EXECUTOR_STEP1_TEXT.to_string()],
                tool_calls: vec![MockOpenAiToolCall {
                    name: EXECUTOR_STEP1_TOOL.to_string(),
                    arguments: json!({
                        "path": "src/components/ThemeToggle.tsx",
                        "content": "// Theme toggle component"
                    }),
                    id: Some("call_create_file_1".to_string()),
                }],
                error: None,
            },
            // Turn 5: Executor reports done
            MockOpenAiTurn {
                text_chunks: vec![EXECUTOR_DONE_RESPONSE.to_string()],
                tool_calls: vec![],
                error: None,
            },
            // Turn 6: Executor receives feedback, runs step 2 (edit_file tool)
            MockOpenAiTurn {
                text_chunks: vec![EXECUTOR_STEP2_TEXT.to_string()],
                tool_calls: vec![MockOpenAiToolCall {
                    name: EXECUTOR_STEP2_TOOL.to_string(),
                    arguments: json!({
                        "path": "src/components/ThemeToggle.tsx",
                        "edit": "Add keyboard shortcut Ctrl+Shift+T"
                    }),
                    id: Some("call_edit_file_1".to_string()),
                }],
                error: None,
            },
            // Turn 7: Executor reports final done
            MockOpenAiTurn {
                text_chunks: vec![EXECUTOR_FINAL_RESPONSE.to_string()],
                tool_calls: vec![],
                error: None,
            },
        ],
        fallback_text: "No more scripted responses.".to_string(),
        chunk_delay_ms: 0,
    }
}

/// Simple handoff script for basic tests - single handoff with tool call
fn simple_handoff_script() -> MockOpenAiScript {
    MockOpenAiScript {
        turns: vec![
            // Turn 1: Agent hands off to sub-agent
            MockOpenAiTurn {
                text_chunks: vec!["I'll hand this off to the specialist.".to_string()],
                tool_calls: vec![MockOpenAiToolCall {
                    name: format!("{}_session_handoff", HANDOFF_SUB_AGENT_NAME),
                    arguments: json!({
                        "prompt": HANDOFF_PROMPT,
                        "session_id": HANDOFF_SESSION_ID,
                    }),
                    id: Some("call_handoff_1".to_string()),
                }],
                error: None,
            },
            // Turn 2: Sub-agent runs a tool
            MockOpenAiTurn {
                text_chunks: vec![EXECUTOR_STEP1_TEXT.to_string()],
                tool_calls: vec![MockOpenAiToolCall {
                    name: HANDOFF_SUB_AGENT_TOOL_NAME.to_string(),
                    arguments: json!({
                        "path": "/tmp"
                    }),
                    id: Some("call_tool_1".to_string()),
                }],
                error: None,
            },
            // Turn 3: Sub-agent responds
            MockOpenAiTurn {
                text_chunks: vec![HANDOFF_FINAL_RESPONSE.to_string()],
                tool_calls: vec![],
                error: None,
            },
        ],
        fallback_text: "No more scripted responses.".to_string(),
        chunk_delay_ms: 0,
    }
}

fn write_handoff_fixture_files(paths: &TestPaths) -> Result<()> {
    write_handoff_fixture_files_with_parent_use_tools(
        paths,
        &format!("{}_session_handoff", HANDOFF_SUB_AGENT_NAME),
    )
}

fn write_handoff_fixture_files_no_acp_server(paths: &TestPaths) -> Result<()> {
    write_handoff_fixture_files_no_acp_server_with_parent_use_tools(
        paths,
        &format!("{}_session_handoff", HANDOFF_SUB_AGENT_NAME),
    )
}

fn write_handoff_fixture_files_with_parent_use_tools(
    paths: &TestPaths,
    parent_use_tools: &str,
) -> Result<()> {
    write_handoff_fixture_files_inner(paths, parent_use_tools, true)
}

fn write_handoff_fixture_files_no_acp_server_with_parent_use_tools(
    paths: &TestPaths,
    parent_use_tools: &str,
) -> Result<()> {
    write_handoff_fixture_files_inner(paths, parent_use_tools, false)
}

fn write_handoff_fixture_files_inner(
    paths: &TestPaths,
    parent_use_tools: &str,
    write_acp_server: bool,
) -> Result<()> {
    std::fs::create_dir_all(&paths.harnx_config_dir)?;

    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));

    std::fs::write(&paths.config_path, "save: false\n")?;

    let clients_dir = paths.harnx_config_dir.join("clients");
    std::fs::create_dir_all(&clients_dir)?;

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

    if write_acp_server {
        let acp_servers_dir = paths.harnx_config_dir.join("acp_servers");
        std::fs::create_dir_all(&acp_servers_dir)?;
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
    }

    std::fs::write(
        paths.agents_dir.join(format!("{}.md", HANDOFF_AGENT_NAME)),
        format!(
            "---\nname: {}\nmodel: mock-llm:test\nuse_tools: {}\n---\nYou are {}. Hand off the task to {} using the handoff tool.\n",
            HANDOFF_AGENT_NAME,
            parent_use_tools,
            HANDOFF_AGENT_NAME,
            HANDOFF_SUB_AGENT_NAME,
        ),
    )?;
    std::fs::write(
        paths
            .agents_dir
            .join(format!("{}.md", HANDOFF_SUB_AGENT_NAME)),
        format!(
            "---\nname: {}\nmodel: mock-llm:test\n---\nYou are {}. {}\n",
            HANDOFF_SUB_AGENT_NAME, HANDOFF_SUB_AGENT_NAME, HANDOFF_SUB_AGENT_SYSTEM_PROMPT,
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

    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));

    let temp = TempDir::new().context("failed to create temp dir")?;
    let mock = MockOpenAiServer::start(nested_script())?;
    let paths = TestPaths::new(temp.path(), mock.port())?;
    write_nested_fixture_files(&paths)?;

    let path_env = format!(
        "{}:{}",
        harnx_bin
            .parent()
            .context("harnx binary missing parent directory")?
            .display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let root = repo_root()?;
    let tmux = match TmuxHarness::new(&root, 120, 80) {
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
        shell_escape(root.to_string_lossy().as_ref())
    ))?;
    tmux.send_keys(&["Enter"])?;
    tmux.wait_for(Duration::from_secs(5), |screen| {
        count_occurrences(screen, "__NESTED_READY__") >= 2
    })?;

    // Start interactive session
    tmux.send_text(&format!(
        "{} -a {} || echo HARNX_EXIT:$?",
        shell_escape(harnx_bin.to_string_lossy().as_ref()),
        NESTED_PARENT_AGENT
    ))?;
    tmux.send_keys(&["Enter"])?;

    tmux.wait_for_contains("Welcome to harnx", Duration::from_secs(15))?;
    tmux.send_text("do nested delegation")?;
    tmux.send_keys(&["Enter"])?;

    let screen = tmux.wait_for_stable(
        Duration::from_secs(30),
        Duration::from_millis(300),
        |screen| screen.contains("Nested task complete"),
    )?;

    let delegate_tool_name = format!("{}_session_prompt", NESTED_SUB_AGENT);
    let nested_delegate_tool_name = format!("{}_session_prompt", NESTED_SUB_SUB_AGENT);
    assert_eq!(
        count_occurrences(&screen, &format!("→ {delegate_tool_name}")),
        1,
        "expected exactly one parent→sub-agent marker:\n{screen}"
    );
    assert_eq!(
        count_occurrences(&screen, &format!("→ {nested_delegate_tool_name}")),
        1,
        "expected exactly one sub-agent→sub-sub-agent marker:\n{screen}"
    );
    assert!(
        count_occurrences(&screen, "Analyst complete") >= 1,
        "expected analyst final message to appear:\n{screen}"
    );
    assert!(
        count_occurrences(&screen, "Researcher complete") >= 1,
        "expected researcher final message to appear:\n{screen}"
    );
    assert_eq!(
        count_occurrences(&screen, "Nested task complete"),
        1,
        "expected exactly one parent final message:\n{screen}"
    );

    let normalized = normalize_spinner_chars(&normalize_uuids(&normalize_screen(&screen)));
    assert_snapshot!("nested_sub_agent_activity_no_duplicates", normalized);

    drop(tmux);
    drop(mock);
    Ok(())
}

fn nested_script() -> MockOpenAiScript {
    // Turn order across all agent sessions:
    // 0 parent     → delegate to researcher
    // 1 researcher → delegate to analyst
    // 2 analyst    → final text
    // 3 researcher → final text after analyst returns
    // 4 parent     → final text after researcher returns
    MockOpenAiScript {
        turns: vec![
            MockOpenAiTurn {
                text_chunks: vec!["Parent delegating to researcher.".to_string()],
                tool_calls: vec![MockOpenAiToolCall {
                    name: format!("{}_session_prompt", NESTED_SUB_AGENT),
                    arguments: json!({"message": "Research this deeply and delegate analysis."}),
                    id: Some("call_nested_parent_1".to_string()),
                }],
                error: None,
            },
            MockOpenAiTurn {
                text_chunks: vec!["Researcher delegating to analyst.".to_string()],
                tool_calls: vec![MockOpenAiToolCall {
                    name: format!("{}_session_prompt", NESTED_SUB_SUB_AGENT),
                    arguments: json!({"message": "Analyze data and report back."}),
                    id: Some("call_nested_researcher_1".to_string()),
                }],
                error: None,
            },
            MockOpenAiTurn {
                text_chunks: vec!["Analyst complete".to_string()],
                tool_calls: vec![],
                error: None,
            },
            MockOpenAiTurn {
                text_chunks: vec!["Researcher complete".to_string()],
                tool_calls: vec![],
                error: None,
            },
            MockOpenAiTurn {
                text_chunks: vec!["Nested task complete".to_string()],
                tool_calls: vec![],
                error: None,
            },
        ],
        fallback_text: "No more scripted responses.".to_string(),
        chunk_delay_ms: 0,
    }
}

fn write_nested_fixture_files(paths: &TestPaths) -> Result<()> {
    std::fs::create_dir_all(&paths.harnx_config_dir)?;

    std::fs::write(&paths.config_path, "save: false\n")?;

    let clients_dir = paths.harnx_config_dir.join("clients");
    std::fs::create_dir_all(&clients_dir)?;

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

    std::fs::write(
        paths.agents_dir.join(format!("{}.md", NESTED_PARENT_AGENT)),
        format!(
            "---\nname: {}\nmodel: mock-llm:test\nuse_tools: {}_session_prompt\n---\nYou are {}. Delegate to {}.\n",
            NESTED_PARENT_AGENT, NESTED_SUB_AGENT, NESTED_PARENT_AGENT, NESTED_SUB_AGENT
        ),
    )?;
    std::fs::write(
        paths.agents_dir.join(format!("{}.md", NESTED_SUB_AGENT)),
        format!(
            "---\nname: {}\nmodel: mock-llm:test\nuse_tools: {}_session_prompt\n---\nYou are {}. Delegate to {}.\n",
            NESTED_SUB_AGENT, NESTED_SUB_SUB_AGENT, NESTED_SUB_AGENT, NESTED_SUB_SUB_AGENT
        ),
    )?;
    std::fs::write(
        paths
            .agents_dir
            .join(format!("{}.md", NESTED_SUB_SUB_AGENT)),
        format!(
            "---\nname: {}\nmodel: mock-llm:test\n---\nYou are {}. Analyze and respond.\n",
            NESTED_SUB_SUB_AGENT, NESTED_SUB_SUB_AGENT
        ),
    )?;
    Ok(())
}

// ── Retry / fallback TUI tests ──────────────────────────────────────────────
//
// These tests verify that retry and fallback warning messages appear correctly
// in the TUI transcript under various failure scenarios.

const RETRY_AGENT_NAME: &str = "retry-agent";

/// Helper: set up a tmux session running harnx with a given mock script and
/// agent markdown frontmatter.  Returns the harness, mock server, and temp dir.
struct RetryTmuxSession {
    _temp: TempDir,
    _mock: MockOpenAiServer,
    tmux: TmuxHarness,
    harnx_bin: PathBuf,
}

#[allow(clippy::type_complexity)]
fn setup_retry_tmux_session(
    test_name: &str,
    script: MockOpenAiScript,
    agent_frontmatter: &str,
    extra_setup: Option<&dyn Fn(&TestPaths) -> Result<()>>,
) -> Result<Option<RetryTmuxSession>> {
    if option_env!("CARGO_BIN_NAME") == Some("harnx") {
        eprintln!("skipping {test_name} in binary test target");
        return Ok(None);
    }
    if !TmuxHarness::is_available() {
        eprintln!("skipping {test_name}: tmux is unavailable");
        return Ok(None);
    }

    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));

    let temp = TempDir::new().context("failed to create temp dir")?;
    let mock = MockOpenAiServer::start(script)?;
    let paths = TestPaths::new(temp.path(), mock.port())?;

    // Write config.yaml
    std::fs::write(&paths.config_path, "save: false\n")?;

    // Write client config with two models: "primary" and "fallback"
    let clients_dir = paths.harnx_config_dir.join("clients");
    std::fs::create_dir_all(&clients_dir)?;
    let client_yaml = format!(
        r#"type: openai-compatible
name: mock-llm
api_base: "http://127.0.0.1:{}/v1"
api_key: dummy
models:
  - name: primary
    max_input_tokens: 32000
    max_output_tokens: 4096
    supports_tool_use: true
  - name: fallback
    max_input_tokens: 32000
    max_output_tokens: 4096
    supports_tool_use: true
"#,
        paths.port
    );
    std::fs::write(clients_dir.join("mock-llm.yaml"), client_yaml)?;

    // Write agent markdown
    std::fs::write(
        paths.agents_dir.join(format!("{RETRY_AGENT_NAME}.md")),
        agent_frontmatter,
    )?;

    if let Some(setup_fn) = extra_setup {
        setup_fn(&paths)?;
    }

    let path_env = format!(
        "{}:{}",
        harnx_bin
            .parent()
            .context("harnx binary missing parent directory")?
            .display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let root = repo_root()?;
    let tmux = match TmuxHarness::new(&root, 120, 50) {
        Ok(tmux) => tmux,
        Err(err) => {
            eprintln!("skipping {test_name}: tmux unusable ({err:#})");
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
        "export PATH={} && cd {}; printf '__RETRY_READY__\\n'",
        shell_escape(&path_env),
        shell_escape(root.to_string_lossy().as_ref())
    ))?;
    tmux.send_keys(&["Enter"])?;
    tmux.wait_for(Duration::from_secs(5), |screen| {
        count_occurrences(screen, "__RETRY_READY__") >= 2
    })?;

    Ok(Some(RetryTmuxSession {
        _temp: temp,
        _mock: mock,
        tmux,
        harnx_bin,
    }))
}

/// Test: all retries and fallbacks fail — error messages appear in transcript.
#[test]
fn retry_all_fail_shows_warnings_in_tui() -> Result<()> {
    // Script: 6 error turns (3 retries for primary + 3 retries for fallback)
    let script = MockOpenAiScript {
        turns: (0..6)
            .map(|_| MockOpenAiTurn {
                text_chunks: vec![],
                tool_calls: vec![],
                error: Some(MockOpenAiError {
                    status: 500,
                    message: "Internal Server Error".to_string(),
                    error_type: "server_error".to_string(),
                    headers: vec![],
                }),
            })
            .collect(),
        fallback_text: "Should not reach here.".to_string(),
        chunk_delay_ms: 0,
    };

    let agent_md = format!(
        "---\nname: {RETRY_AGENT_NAME}\nmodel: mock-llm:primary\nmodel_fallbacks:\n  - mock-llm:fallback\nretry:\n  attempts: 3\n  initial_delay_ms: 10\n  max_delay_ms: 50\n---\nYou are a test agent.\n"
    );

    let session = setup_retry_tmux_session(
        "retry_all_fail_shows_warnings_in_tui",
        script,
        &agent_md,
        None,
    )?;
    let session = match session {
        Some(s) => s,
        None => return Ok(()),
    };

    // Launch agent
    session.tmux.send_text(&format!(
        "{} -a {RETRY_AGENT_NAME} || echo HARNX_EXIT:$?",
        shell_escape(session.harnx_bin.to_string_lossy().as_ref()),
    ))?;
    session.tmux.send_keys(&["Enter"])?;
    session
        .tmux
        .wait_for_contains("Welcome to harnx", Duration::from_secs(15))?;

    session.tmux.send_text("hello")?;
    session.tmux.send_keys(&["Enter"])?;

    // Wait for the error to appear — all models should fail and we should see
    // retry warnings and fallback transition messages.
    let screen = session.tmux.wait_for_stable(
        Duration::from_secs(30),
        Duration::from_millis(500),
        |screen| {
            screen.contains("error: Failed to call chat-completions api")
                || screen.contains("HARNX_EXIT:")
        },
    )?;

    // Verify retry warnings appeared in the transcript
    assert!(
        screen.contains("Retryable error"),
        "retry warning not visible in TUI transcript:\n{screen}"
    );

    // Verify fallback transition message appeared
    assert!(
        screen.contains("exhausted retries"),
        "fallback exhaustion message not visible in TUI transcript:\n{screen}"
    );

    let normalized = normalize_spinner_chars(&normalize_uuids(&normalize_screen(&screen)));
    assert_snapshot!("retry_all_fail_shows_warnings_in_tui", normalized);

    Ok(())
}

/// Test: succeed after retry — first attempt fails, second succeeds.
#[test]
fn retry_succeed_after_retry_shows_warning_then_response() -> Result<()> {
    let script = MockOpenAiScript {
        turns: vec![
            // First attempt: error
            MockOpenAiTurn {
                text_chunks: vec![],
                tool_calls: vec![],
                error: Some(MockOpenAiError {
                    status: 500,
                    message: "Temporary failure".to_string(),
                    error_type: "server_error".to_string(),
                    headers: vec![],
                }),
            },
            // Second attempt: success
            MockOpenAiTurn {
                text_chunks: vec![
                    "RETRY_SUCCESS_RESPONSE: Hello from the retried model!".to_string()
                ],
                tool_calls: vec![],
                error: None,
            },
        ],
        fallback_text: "Should not reach here.".to_string(),
        chunk_delay_ms: 0,
    };

    let agent_md = format!(
        "---\nname: {RETRY_AGENT_NAME}\nmodel: mock-llm:primary\nretry:\n  attempts: 3\n  initial_delay_ms: 10\n  max_delay_ms: 50\n---\nYou are a test agent.\n"
    );

    let session = setup_retry_tmux_session(
        "retry_succeed_after_retry_shows_warning_then_response",
        script,
        &agent_md,
        None,
    )?;
    let session = match session {
        Some(s) => s,
        None => return Ok(()),
    };

    session.tmux.send_text(&format!(
        "{} -a {RETRY_AGENT_NAME} || echo HARNX_EXIT:$?",
        shell_escape(session.harnx_bin.to_string_lossy().as_ref()),
    ))?;
    session.tmux.send_keys(&["Enter"])?;
    session
        .tmux
        .wait_for_contains("Welcome to harnx", Duration::from_secs(15))?;

    session.tmux.send_text("hello")?;
    session.tmux.send_keys(&["Enter"])?;

    let screen = session.tmux.wait_for_stable(
        Duration::from_secs(30),
        Duration::from_millis(500),
        |screen| screen.contains("RETRY_SUCCESS_RESPONSE"),
    )?;

    // Verify the retry warning appeared before the successful response
    assert!(
        screen.contains("Retryable error"),
        "retry warning not visible in TUI transcript:\n{screen}"
    );
    assert!(
        screen.contains("RETRY_SUCCESS_RESPONSE"),
        "successful response not visible after retry:\n{screen}"
    );

    let normalized = normalize_spinner_chars(&normalize_uuids(&normalize_screen(&screen)));
    assert_snapshot!(
        "retry_succeed_after_retry_shows_warning_then_response",
        normalized
    );

    Ok(())
}

/// Test: succeed after fallback — primary model fails all retries, fallback succeeds.
#[test]
fn retry_succeed_after_fallback_shows_transition() -> Result<()> {
    let script = MockOpenAiScript {
        turns: vec![
            // Primary model: 2 attempts both fail (retry attempts = 2)
            MockOpenAiTurn {
                text_chunks: vec![],
                tool_calls: vec![],
                error: Some(MockOpenAiError {
                    status: 500,
                    message: "Primary down".to_string(),
                    error_type: "server_error".to_string(),
                    headers: vec![],
                }),
            },
            MockOpenAiTurn {
                text_chunks: vec![],
                tool_calls: vec![],
                error: Some(MockOpenAiError {
                    status: 500,
                    message: "Primary still down".to_string(),
                    error_type: "server_error".to_string(),
                    headers: vec![],
                }),
            },
            // Fallback model: succeeds on first attempt
            MockOpenAiTurn {
                text_chunks: vec!["FALLBACK_SUCCESS: The fallback model handled it!".to_string()],
                tool_calls: vec![],
                error: None,
            },
        ],
        fallback_text: "Should not reach here.".to_string(),
        chunk_delay_ms: 0,
    };

    let agent_md = format!(
        "---\nname: {RETRY_AGENT_NAME}\nmodel: mock-llm:primary\nmodel_fallbacks:\n  - mock-llm:fallback\nretry:\n  attempts: 2\n  initial_delay_ms: 10\n  max_delay_ms: 50\n---\nYou are a test agent.\n"
    );

    let session = setup_retry_tmux_session(
        "retry_succeed_after_fallback_shows_transition",
        script,
        &agent_md,
        None,
    )?;
    let session = match session {
        Some(s) => s,
        None => return Ok(()),
    };

    session.tmux.send_text(&format!(
        "{} -a {RETRY_AGENT_NAME} || echo HARNX_EXIT:$?",
        shell_escape(session.harnx_bin.to_string_lossy().as_ref()),
    ))?;
    session.tmux.send_keys(&["Enter"])?;
    session
        .tmux
        .wait_for_contains("Welcome to harnx", Duration::from_secs(15))?;

    session.tmux.send_text("hello")?;
    session.tmux.send_keys(&["Enter"])?;

    let screen = session.tmux.wait_for_stable(
        Duration::from_secs(30),
        Duration::from_millis(500),
        |screen| screen.contains("FALLBACK_SUCCESS"),
    )?;

    // Verify retry warnings for primary model
    assert!(
        screen.contains("Retryable error"),
        "retry warning for primary model not visible:\n{screen}"
    );
    // Verify fallback transition message
    assert!(
        screen.contains("exhausted retries"),
        "fallback transition message not visible:\n{screen}"
    );
    // Verify fallback response appeared
    assert!(
        screen.contains("FALLBACK_SUCCESS"),
        "fallback model response not visible:\n{screen}"
    );

    let normalized = normalize_spinner_chars(&normalize_uuids(&normalize_screen(&screen)));
    assert_snapshot!("retry_succeed_after_fallback_shows_transition", normalized);

    Ok(())
}
