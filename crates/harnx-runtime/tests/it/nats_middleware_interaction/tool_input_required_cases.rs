//! T4.3 Part A: native tools return `input_required` and suspend through the
//! D6 durable interaction path, then resume with the user's answers beside
//! the arguments. Cases cover the no-middleware opt-in, worker movement,
//! repeated challenges, and middleware/tool identity distinctness.
use super::harness::{extract_tool_results, single_call_model, Harness};
use anyhow::{Context, Result};
use harnx_core::{
    instance::ServerScope, interaction::InteractionResponse, session::SessionLogEntry,
};
use harnx_runtime::nats_session_log::NatsSessionLog;
use harnx_toolset::server_identity_token;
use harnx_toolset_server::{registration_key, TOOL_REGISTRY_BUCKET};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    time::{Duration, Instant},
};

struct Requested {
    seq: u64,
    interaction_id: String,
    producer: String,
    key: String,
    kind: String,
    payload: Value,
    step_path_len: usize,
}

async fn load_entries(harness: &Harness) -> Result<Vec<(u64, SessionLogEntry)>> {
    let log =
        NatsSessionLog::new_with_replicas(harness.js.clone(), harness.session.storage_key(), 1);
    log.load_events_async().await
}

fn requested_of(entries: &[(u64, SessionLogEntry)]) -> Vec<Requested> {
    entries
        .iter()
        .filter_map(|(seq, entry)| {
            let SessionLogEntry::InteractionRequested {
                interaction_id,
                producer,
                step_path,
                key,
                kind,
                payload,
                ..
            } = entry
            else {
                return None;
            };
            Some(Requested {
                seq: *seq,
                interaction_id: interaction_id.clone(),
                producer: producer.clone(),
                key: key.clone(),
                kind: kind.clone(),
                payload: payload.clone(),
                step_path_len: step_path.len(),
            })
        })
        .collect()
}

async fn wait_for_requests(harness: &Harness, count: usize) -> Result<Vec<Requested>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let requested = requested_of(&load_entries(harness).await?);
        if requested.len() >= count {
            return Ok(requested);
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {count} interaction requests"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn respond(
    harness: &Harness,
    requested: &Requested,
    action: &str,
    data: serde_json::Map<String, Value>,
) -> Result<harnx_runtime::interactions::InteractionDecisionOutcome> {
    harness
        .session
        .decide_interaction(
            &requested.interaction_id,
            requested.seq,
            &InteractionResponse {
                action: action.into(),
                data,
                note: None,
            },
        )
        .await
}

fn tool_output(entries: &[(u64, SessionLogEntry)], call_id: &str) -> Result<Value> {
    let results = extract_tool_results(entries);
    let result = results
        .iter()
        .find(|r| r.id.as_deref() == Some(call_id))
        .with_context(|| format!("no tool result for {call_id}"))?;
    assert_ne!(
        result.output.get("is_error"),
        Some(&json!(true)),
        "tool result must not be an error: {:?}",
        result.output
    );
    Ok(result.output.clone())
}

/// Start a round calling the `probe_challenge` tool in `mode` (no middleware)
/// and wait for its first suspension.
async fn start_challenge_round(
    mode: &'static str,
    value: &'static str,
    call_id: &'static str,
    activate: &'static str,
) -> Result<Harness> {
    let model = single_call_model(
        "probe_challenge",
        json!({"value": value, "mode": mode}),
        call_id,
    );
    let (harness, _, _, _) = Harness::start_and_suspend("", model, activate).await?;
    Ok(harness)
}

/// A challenge from an opted-in tool suspends through the durable path with
/// no middleware configured, and the rerun delivers the answer beside the
/// arguments — the model-authored arguments are unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_input_required_confirm_suspends_and_resumes_without_middleware() -> Result<()> {
    let model = single_call_model(
        "probe_challenge",
        json!({"value": "go", "mode": "confirm"}),
        "call-challenge",
    );
    let (harness, _, _, call_id) =
        Harness::start_and_suspend("", model, "start tool challenge").await?;
    assert_eq!(call_id, "call-challenge");

    let requested = wait_for_requests(&harness, 1).await?;
    assert_eq!(requested.len(), 1, "exactly one interaction expected");
    let request = &requested[0];
    assert_eq!(request.kind, "confirm");
    assert_eq!(request.key, "ask-key");
    assert_eq!(request.step_path_len, 0, "no middleware frames configured");
    assert!(
        request.producer.starts_with("tool:") && request.producer.ends_with("/challenge"),
        "producer must be infrastructure-derived tool identity, got {}",
        request.producer
    );
    assert_eq!(
        request.payload["createSurface"]["components"][0]["text"],
        json!("Allow the probe to proceed?")
    );

    assert!(
        respond(&harness, request, "approve", serde_json::Map::new())
            .await?
            .is_applied()
    );
    let entries = harness.wait_for_turn_end().await?;

    // Two invocations: the challenge, then the rerun with the answer.
    let seen = harness.probe.challenge_seen.lock().clone();
    assert_eq!(seen.len(), 2, "challenge tool runs twice: {seen:?}");
    assert!(seen[0]["responses"]
        .as_object()
        .is_some_and(|m| m.is_empty()));
    assert_eq!(seen[0]["state"], Value::Null);
    let rerun = &seen[1];
    assert_eq!(
        rerun["responses"]["ask-key"]["action"], "approve",
        "answer arrives beside the arguments: {rerun:?}"
    );
    assert_eq!(
        rerun["state"],
        json!({"round": 1, "token": "probe-state-1"})
    );

    // The model-authored arguments reached the tool unchanged both times.
    let calls = harness.probe.calls.lock().clone();
    let challenge_calls: Vec<&Value> = calls
        .iter()
        .filter(|(tool, _)| tool == "probe_challenge")
        .map(|(_, args)| args)
        .collect();
    assert_eq!(challenge_calls.len(), 2);
    for args in challenge_calls {
        assert_eq!(
            *args,
            json!({"value": "go", "mode": "confirm"}),
            "arguments never carry challenge traffic: {args:?}"
        );
    }

    let output = tool_output(&entries, "call-challenge")?;
    assert_eq!(
        output["state"],
        json!({"round": 1, "token": "probe-state-1"})
    );
    assert_eq!(output["responses"]["ask-key"]["action"], "approve");
    Ok(())
}

/// An A2UI form challenge reuses the T4.1 surface validation and stamping,
/// and the form data reaches the tool through the invocation context.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_input_required_a2ui_form_delivers_response_data() -> Result<()> {
    let harness =
        start_challenge_round("a2ui", "form", "call-form", "start a2ui challenge").await?;

    let requested = wait_for_requests(&harness, 1).await?;
    let request = &requested[0];
    assert_eq!(request.kind, "a2ui");
    assert_eq!(request.key, "form-key");
    assert!(request.producer.ends_with("/challenge"));
    // The runtime stamped a fresh surface id into the tool's envelope.
    let surface_id = request.payload["createSurface"]["surfaceId"]
        .as_str()
        .context("stamped surface id")?;
    assert!(!surface_id.is_empty());

    let mut data = serde_json::Map::new();
    data.insert("choice".into(), json!(["b"]));
    data.insert("name".into(), json!("Ada"));
    assert!(respond(&harness, request, "submit", data)
        .await?
        .is_applied());
    let entries = harness.wait_for_turn_end().await?;

    let seen = harness.probe.challenge_seen.lock().clone();
    assert_eq!(seen.len(), 2, "form challenge then rerun: {seen:?}");
    let rerun = &seen[1];
    assert_eq!(rerun["responses"]["form-key"]["action"], "submit");
    assert_eq!(
        rerun["responses"]["form-key"]["data"]["choice"],
        json!(["b"])
    );
    assert_eq!(rerun["responses"]["form-key"]["data"]["name"], json!("Ada"));
    assert_eq!(
        rerun["state"],
        json!({"round": 1, "token": "probe-state-1"})
    );

    let output = tool_output(&entries, "call-form")?;
    assert_eq!(
        output["responses"]["form-key"]["data"]["name"],
        json!("Ada")
    );
    Ok(())
}

/// The checkpointed request state survives the session moving to a
/// replacement worker before the user answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_input_required_resume_survives_worker_move() -> Result<()> {
    let model = single_call_model(
        "probe_challenge",
        json!({"value": "move", "mode": "confirm"}),
        "call-move",
    );
    let (mut harness, _, _, _) =
        Harness::start_and_suspend("", model, "start pre-move challenge").await?;

    let requested = wait_for_requests(&harness, 1).await?;

    // Move the session to a different worker while suspended.
    harness
        .restart_worker_with_identity("worker-tool-input-required-b")
        .await?;

    assert!(
        respond(&harness, &requested[0], "approve", serde_json::Map::new())
            .await?
            .is_applied()
    );
    let entries = harness.wait_for_turn_end().await?;

    let seen = harness.probe.challenge_seen.lock().clone();
    assert_eq!(seen.len(), 2, "challenge then post-move rerun: {seen:?}");
    let rerun = &seen[1];
    assert_eq!(
        rerun["state"],
        json!({"round": 1, "token": "probe-state-1"}),
        "request state must survive via the checkpoint store: {rerun:?}"
    );
    assert_eq!(rerun["responses"]["ask-key"]["action"], "approve");

    let output = tool_output(&entries, "call-move")?;
    assert_eq!(
        output["state"],
        json!({"round": 1, "token": "probe-state-1"})
    );
    Ok(())
}

/// A tool that challenges twice prompts twice; each rerun sees only the
/// latest challenge's responses and state (D8 repeated-challenge rule).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_input_required_double_challenge_sends_only_latest_responses() -> Result<()> {
    let harness =
        start_challenge_round("double", "twice", "call-double", "start double challenge").await?;

    let first_batch = wait_for_requests(&harness, 1).await?;
    let first = &first_batch[0];
    assert_eq!(first.kind, "confirm");
    assert_eq!(first.key, "step-1");
    assert!(respond(&harness, first, "approve", serde_json::Map::new())
        .await?
        .is_applied());

    // The rerun answers step-1 and immediately challenges again.
    let second_batch = wait_for_requests(&harness, 2).await?;
    let second = &second_batch[1];
    assert_eq!(second.kind, "confirm");
    assert_eq!(second.key, "step-2");
    assert_ne!(
        first.interaction_id, second.interaction_id,
        "each challenge is its own interaction"
    );
    assert_eq!(second.producer, first.producer);

    // What the tool saw on the step-1 rerun: only step-1's answer and state.
    let seen_mid = harness.probe.challenge_seen.lock().clone();
    assert_eq!(
        seen_mid.len(),
        2,
        "first rerun happened before challenge two"
    );
    assert_eq!(seen_mid[1]["responses"]["step-1"]["action"], "approve");
    assert!(!seen_mid[1]["responses"]
        .as_object()
        .expect("responses object")
        .contains_key("step-2"));
    assert_eq!(
        seen_mid[1]["state"],
        json!({"round": 1, "token": "probe-state-1"})
    );

    assert!(respond(&harness, second, "approve", serde_json::Map::new())
        .await?
        .is_applied());
    let entries = harness.wait_for_turn_end().await?;

    // Final rerun: only step-2's answer and state, never step-1's.
    let seen = harness.probe.challenge_seen.lock().clone();
    assert_eq!(seen.len(), 3, "two challenges plus final rerun: {seen:?}");
    let final_seen = &seen[2];
    let responses = final_seen["responses"].as_object().expect("responses");
    assert_eq!(
        BTreeMap::from_iter(responses.iter().map(|(k, _)| (k.clone(), ()))),
        BTreeMap::from([("step-2".to_string(), ())]),
        "only the latest challenge's responses arrive: {final_seen:?}"
    );
    assert_eq!(final_seen["responses"]["step-2"]["action"], "approve");
    assert_eq!(
        final_seen["state"],
        json!({"round": 2, "token": "probe-state-2"})
    );

    let output = tool_output(&entries, "call-double")?;
    assert_eq!(
        output["state"],
        json!({"round": 2, "token": "probe-state-2"})
    );
    let output_responses = output["responses"].as_object().expect("output responses");
    assert_eq!(output_responses.len(), 1);
    assert!(output_responses.contains_key("step-2"));
    Ok(())
}

/// A middleware interaction and a tool interaction in the same call have
/// distinct identities, and D4 holds: the middleware wrapping a tool that
/// returns input-required sees the typed suspension propagate, not a
/// catchable error — the call finishes with a real result once answered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_input_required_and_middleware_interaction_have_distinct_identities() -> Result<()> {
    const GATED: &str = r#"
middleware:
- name: gate
  match: '^probe_challenge$'
  bash: |
    if confirm --id gate 'Middleware gate?'; then
      next
    else
      deny 'user denied'
    fi
"#;
    let model = single_call_model(
        "probe_challenge",
        json!({"value": "gated", "mode": "confirm"}),
        "call-gated",
    );
    let (harness, _, _, _) =
        Harness::start_and_suspend(GATED, model, "start gated challenge").await?;

    // First suspension belongs to the middleware frame.
    let first_batch = wait_for_requests(&harness, 1).await?;
    let gate = &first_batch[0];
    assert_eq!(gate.key, "gate");
    assert!(
        !gate.producer.starts_with("tool:"),
        "middleware interaction is not tool-owned: {}",
        gate.producer
    );
    assert!(respond(&harness, gate, "approve", serde_json::Map::new())
        .await?
        .is_applied());

    // Second suspension: the tool challenges inside the middleware frame.
    let second_batch = wait_for_requests(&harness, 2).await?;
    let tool = &second_batch[1];
    assert_eq!(tool.key, "ask-key");
    assert!(
        tool.producer.starts_with("tool:") && tool.producer.ends_with("/challenge"),
        "tool interaction is tool-owned: {}",
        tool.producer
    );
    assert_ne!(
        gate.producer, tool.producer,
        "middleware and tool interactions have distinct producers"
    );
    assert_ne!(gate.interaction_id, tool.interaction_id);
    assert!(
        tool.step_path_len > 0,
        "the tool's identity includes the enclosing middleware frame's step path"
    );
    assert_eq!(
        tool.step_path_len,
        gate.step_path_len + 1,
        "the tool challenges inside the gate frame's `next` step, one deeper than the gate's own confirm"
    );

    assert!(respond(&harness, tool, "approve", serde_json::Map::new())
        .await?
        .is_applied());
    let entries = harness.wait_for_turn_end().await?;

    // D4: the downstream InputRequired propagated as a typed suspension;
    // once answered the call completes with a normal result, not an error.
    let output = tool_output(&entries, "call-gated")?;
    assert_eq!(
        output["state"],
        json!({"round": 1, "token": "probe-state-1"})
    );
    assert_eq!(output["responses"]["ask-key"]["action"], "approve");

    // The middleware frame saw the final result: exactly one ToolResults.
    let results = extract_tool_results(&entries);
    assert_eq!(results.len(), 1);
    Ok(())
}

/// The MRTR round cap (SEP-2322): a tool that challenges on every rerun is
/// cut off after `MAX_TOOL_INPUT_ROUNDS` challenges with a visible error
/// result instead of suspending forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_input_required_round_cap_gives_up_after_ten_challenges() -> Result<()> {
    let harness =
        start_challenge_round("loop", "forever", "call-loop", "start loop challenge").await?;

    for round in 1..=harnx_toolset::MAX_TOOL_INPUT_ROUNDS {
        let batch = wait_for_requests(&harness, round).await?;
        let request = &batch[round - 1];
        assert_eq!(request.key, format!("round-{round}"));
        assert!(
            respond(&harness, request, "approve", serde_json::Map::new())
                .await?
                .is_applied()
        );
    }

    // The tool challenges again on its 11th invocation, but the runtime has
    // already journaled 10 of its challenges: the call finishes with an
    // error result instead of an 11th suspension.
    let entries = harness.wait_for_turn_end().await?;
    assert_eq!(
        requested_of(&entries).len(),
        harnx_toolset::MAX_TOOL_INPUT_ROUNDS,
        "exactly the capped number of challenges is journaled"
    );
    let seen = harness.probe.challenge_seen.lock().clone();
    assert_eq!(
        seen.len(),
        harnx_toolset::MAX_TOOL_INPUT_ROUNDS + 1,
        "the tool ran once more, then the runtime cut it off: {seen:?}"
    );

    let results = extract_tool_results(&entries);
    let result = results
        .iter()
        .find(|r| r.id.as_deref() == Some("call-loop"))
        .context("no tool result for call-loop")?;
    assert_eq!(
        result.output["is_error"],
        json!(true),
        "the cap yields an error result, not a success: {}",
        result.output
    );
    assert!(
        result.output["error"]
            .as_str()
            .is_some_and(|error| error.contains("10 times")),
        "the error names the cap: {}",
        result.output
    );
    Ok(())
}

/// The T4.3 Part B acceptance case: a tool behind a bridged external MCP
/// server suspends through the same durable path when it returns MRTR
/// `input_required`, the user's answer resumes the call, and the MCP server
/// receives `inputResponses` and `requestState` beside the unchanged
/// arguments — only the latest challenge's, on repeated rounds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_input_required_mcp_bridged_tool_suspends_and_resumes_over_d6() -> Result<()> {
    let model = single_call_model("ask_mcp", json!({"value": "bridged"}), "call-mcp-bridge");
    let harness = Harness::required_start("", model).await?;
    let bridge = BridgedMrtrServer::start(&harness).await?;

    harness.activate("start bridged challenge").await?;

    // The first MRTR challenge suspends the call through the durable path,
    // with the infrastructure-derived bridged-tool identity.
    let first_batch = wait_for_requests(&harness, 1).await?;
    let first = &first_batch[0];
    assert_eq!(first.kind, "confirm");
    assert_eq!(first.key, "mc-ask-1");
    assert!(
        first.producer.starts_with("tool:")
            && first.producer.contains(BRIDGE_SERVER)
            && first.producer.ends_with("/ask_mcp"),
        "producer must be the bridged tool's identity, got {}",
        first.producer
    );
    assert!(respond(&harness, first, "approve", serde_json::Map::new())
        .await?
        .is_applied());

    // The rerun answers mc-ask-1, and the tool challenges again.
    let second_batch = wait_for_requests(&harness, 2).await?;
    let second = &second_batch[1];
    assert_eq!(second.kind, "confirm");
    assert_eq!(second.key, "mc-ask-2");
    assert_eq!(second.producer, first.producer);
    assert!(respond(&harness, second, "approve", serde_json::Map::new())
        .await?
        .is_applied());

    let entries = harness.wait_for_turn_end().await?;
    let output = tool_output(&entries, "call-mcp-bridge")?;
    assert_eq!(output["content"][0]["text"], json!("bridged done"));

    // What the MCP server received: arguments unchanged on every round, and
    // only the latest challenge's responses and state beside them.
    let calls = bridge.tool_calls()?;
    assert_eq!(calls.len(), 3, "two challenge rounds plus the final call");
    for (round, call) in calls.iter().enumerate() {
        assert_eq!(
            call["arguments"],
            json!({"value": "bridged"}),
            "arguments unchanged on round {round}: {call}"
        );
    }
    assert!(calls[0].get("inputResponses").is_none());
    assert!(calls[0].get("requestState").is_none());
    assert_eq!(
        calls[1]["inputResponses"]["mc-ask-1"]["action"],
        json!("accept")
    );
    assert_eq!(
        calls[1]["inputResponses"]["mc-ask-1"]["_meta"]["harnx:action"],
        json!("approve")
    );
    assert_eq!(
        calls[1]["inputResponses"]
            .as_object()
            .map(serde_json::Map::len),
        Some(1),
        "only the latest challenge's responses: {}",
        calls[1]["inputResponses"]
    );
    assert_eq!(calls[1]["requestState"], json!("{\"round\":1}"));
    assert_eq!(
        calls[2]["inputResponses"]["mc-ask-2"]["_meta"]["harnx:action"],
        json!("approve")
    );
    assert_eq!(
        calls[2]["inputResponses"]
            .as_object()
            .map(serde_json::Map::len),
        Some(1),
        "only the latest challenge's responses: {}",
        calls[2]["inputResponses"]
    );
    assert_eq!(calls[2]["requestState"], json!("{\"round\":2}"));
    Ok(())
}

/// The server name the spawned bridge registers under.
const BRIDGE_SERVER: &str = "mcp-input-test";

/// The mock MCP server's script: two MRTR `input_required` challenges and
/// then a final result, for the bridged acceptance case.
const MRTR_SCRIPT: &str = r#"
tools:
  - name: ask_mcp
    description: Bridged tool that asks for input twice, then finishes.
input_required:
  - key: mc-ask-1
    message: "Bridged round 1?"
    request_state: {"round": 1}
  - key: mc-ask-2
    message: "Bridged round 2?"
    request_state: {"round": 2}
responses:
  - "bridged done"
"#;

/// A spawned `harnx-mcp-bridge` in front of the SDK-backed mock MCP server,
/// registered in the harness's scope, keeping the mock's request audit log.
struct BridgedMrtrServer {
    child: tokio::process::Child,
    request_log: PathBuf,
}

impl BridgedMrtrServer {
    /// Spawn the bridge and wait until its toolset is registered in the
    /// scope the harness's worker discovers tools in. The harness owns the
    /// broker and scope and exports them in-process; the spawned children
    /// inherit the environment.
    async fn start(harness: &Harness) -> Result<Self> {
        let root = harness._root.path();
        let script = root.join("mrtr-script.yaml");
        std::fs::write(&script, MRTR_SCRIPT)?;
        let request_log = root.join("mrtr-requests.jsonl");

        let mut command = tokio::process::Command::new(binary("harnx-mcp-bridge")?);
        command
            .args(["--name", BRIDGE_SERVER, "--"])
            .arg(binary("harnx-mock-mcp")?)
            .arg("--script")
            .arg(&script)
            .arg("--request-log")
            .arg(&request_log)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let mut server = Self {
            child: command.spawn()?,
            request_log,
        };

        let scope =
            ServerScope::from_string(std::env::var(harnx_core::instance::HARNX_SERVER_SCOPE)?);
        let identity = server_identity_token(None, "", BRIDGE_SERVER);
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = server.child.try_wait()? {
                anyhow::bail!("bridge exited before registering: {status}");
            }
            let registered = harness
                .js
                .get_key_value(TOOL_REGISTRY_BUCKET)
                .await?
                .get(registration_key(&scope, &identity))
                .await?
                .is_some();
            if registered {
                return Ok(server);
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "bridge never registered its toolset"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// The `tools/call` requests the mock MCP server actually received.
    fn tool_calls(&self) -> Result<Vec<Value>> {
        let entries = std::fs::read_to_string(&self.request_log)?
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_str::<Value>(line).with_context(|| line.to_owned()))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(entries
            .into_iter()
            .filter(|entry| entry["method"] == "tools/call")
            .map(|entry| entry["params"].clone())
            .collect())
    }
}

/// Resolve a sibling workspace binary from the test executable, like the
/// result-fixture bridge tests do.
fn binary(name: &str) -> Result<PathBuf> {
    let mut path = std::env::current_exe()?;
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.push(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    anyhow::ensure!(path.is_file(), "build {name} before this integration test");
    Ok(path)
}
