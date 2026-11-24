//! Scripted streaming payloads and pacing for real worker tests.
use super::*;
use harnx_runtime::config::Config;

pub(super) struct FramePlan {
    pub frames: Vec<String>,
    held: bool,
    many: bool,
}

impl FramePlan {
    pub fn new(llm: &Llm, body: &Value, first: bool) -> Self {
        let many = matches!(llm.script, Script::Many | Script::Large | Script::Boundary);
        let counted_tool = needs_counted_tool(llm.script, body);
        let frames = select_frames(llm, first, many, counted_tool);
        // Hold text after its first SSE token until an artifact is observed.
        let held = needs_hold(llm, counted_tool, first);
        Self { frames, held, many }
    }

    pub fn waits_before(&self, index: usize) -> bool {
        if self.held && index == 1 {
            return true;
        }
        if !self.many {
            return false;
        }
        index > 0 && index <= self.frames.len() - 2
    }
}

fn needs_hold(llm: &Llm, counted_tool: bool, first: bool) -> bool {
    let script = llm.script;
    match script {
        Script::Text => true,
        Script::ConfirmHeld => counted_tool,
        Script::CountedTool => !counted_tool,
        // Hold the first request's tool-call completion until the test has
        // steered a message into the running turn; the follow-up request
        // after the tool round carries the steered text and streams out.
        // The steered row is that request's last message, so the role-based
        // counted-tool probe would misfire; key off the request index.
        Script::Steering => first,
        // Hold the root and the steered continuation rounds so a test can
        // place owner loss inside the drain window; later rounds (a fresh
        // task after settle) stream freely.
        Script::SteeringHeld => llm.requests.lock().len() <= 2,
        Script::SteeringFail => first,
        // Hold the root text round so the test can steer into the live
        // turn; the continuation round streams its approval-needing tool
        // call unheld, and the post-approval round answers freely.
        Script::SteeringApproval => first,
        _ => false,
    }
}

fn needs_counted_tool(script: Script, body: &Value) -> bool {
    matches!(
        script,
        Script::CountedTool
            | Script::ConfirmTool
            | Script::ConfirmHeld
            | Script::ConfirmSerial
            | Script::A2uiTool
            | Script::A2uiSerial
    ) && body["messages"]
        .as_array()
        .and_then(|m| m.last())
        .map(|m| m["role"].as_str())
        != Some(Some("tool"))
}

fn needs_handoff(llm: &Llm, first: bool) -> bool {
    match llm.script {
        Script::Tool => first,
        Script::SerialTool => llm.requests.lock().len() <= 2,
        _ => false,
    }
}

/// Frames for the steering script family (A2A issue #2774): `None` means the
/// script is not steered and the caller should keep classifying.
fn steered_frames(llm: &Llm, first: bool) -> Option<Vec<String>> {
    match llm.script {
        // The tool call arrives on the first request; the follow-up request
        // (with the steered row as its last user message) answers as plain
        // text.
        Script::Steering => Some(steering_frames(first)),
        Script::SteeringHeld => Some(steering_held_frames(llm.requests.lock().len())),
        // Only the first request is ever answered; the steered continuation
        // round returns HTTP 400 from the completion handler.
        Script::SteeringFail => Some(text_frames()),
        // Root round answers plain text; the steered continuation round issues
        // a bash_exec call the confirmation middleware parks as a HITL
        // approval; the round after the approval completes the drain.
        Script::SteeringApproval => Some(steering_approval_frames(llm.requests.lock().len())),
        _ => None,
    }
}

fn select_frames(llm: &Llm, first: bool, many: bool, counted_tool: bool) -> Vec<String> {
    if llm.script == Script::Overflow {
        return overflow_frames();
    }
    if many {
        return many_frames(llm.script);
    }
    if let Some(frames) = steered_frames(llm, first) {
        return frames;
    }
    if counted_tool {
        return counted_tool_frames();
    }
    if matches!(
        llm.script,
        Script::SameRoundTools | Script::ConfirmSameRound | Script::A2uiSameRound
    ) && first
    {
        return same_round_frames();
    }
    if llm.script == Script::ShippedDaedalus && first {
        return shipped_handoff_frames();
    }
    if needs_handoff(llm, first) {
        return handoff_frames(first);
    }
    text_frames()
}

fn chunk(delta: Value, finish: Value) -> String {
    json!({"id":"runner-completion", "object":"chat.completion.chunk", "created":0,
        "model":"test", "choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
    .to_string()
}
fn overflow_frames() -> Vec<String> {
    vec![
        chunk(json!({"content":"x".repeat(224 * 1024 + 1)}), Value::Null),
        chunk(json!({}), json!("stop")),
        "[DONE]".into(),
    ]
}

fn many_frames(script: Script) -> Vec<String> {
    let text = "x".repeat(match script {
        Script::Large => 1024,
        Script::Boundary => 3324,
        _ => 1,
    });
    let mut frames = vec![
        chunk(json!({"content":text}), Value::Null);
        harnx_a2a_server::runner::EVENT_CAPACITY + 5
    ];
    if script == Script::Boundary {
        frames[68] = chunk(json!({"content":"x".repeat(3344)}), Value::Null);
    }
    frames.push(chunk(json!({}), json!("stop")));
    frames.push("[DONE]".into());
    frames
}

fn counted_tool_frames() -> Vec<String> {
    let counter = Config::config_dir().join("executions");
    let command = format!(
        "printf 'execution\\n' >> {}",
        shell_words::quote(counter.to_str().expect("counter path"))
    );
    vec![
        chunk(
            json!({"tool_calls":[{"index":0,"id":"counted-call","type":"function",
            "function":{"name":"bash_exec","arguments":json!({"command":command}).to_string()}}]}),
            Value::Null,
        ),
        chunk(json!({}), json!("tool_calls")),
        "[DONE]".into(),
    ]
}

fn same_round_frames() -> Vec<String> {
    let mut frames = vec![chunk(json!({"content":"Before approval. "}), Value::Null)];
    // Separate indexed deltas still form one completion/ToolCalls round.
    for (index, id) in ["first-approval", "second-approval"]
        .into_iter()
        .enumerate()
    {
        frames.push(chunk(
            json!({"tool_calls":[{"index":index,"id":id,"type":"function",
            "function":{"name":"time_get_current_time","arguments":"{\"timezone\":\"UTC\"}"}}]}),
            Value::Null,
        ));
    }
    frames.push(chunk(json!({}), json!("tool_calls")));
    frames.push("[DONE]".into());
    frames
}

fn handoff_frames(first: bool) -> Vec<String> {
    let id = if first {
        "runner-handoff"
    } else {
        "runner-handoff-2"
    };
    let text = if first {
        "Before approval. "
    } else {
        "Between approvals. "
    };
    vec![
        chunk(json!({"content":text}), Value::Null),
        chunk(
            json!({"tool_calls":[{"index":0,"id":id, "type":"function",
            "function":{"name":"target_session_handoff", "arguments":"{\"prompt\":\"must not run\",\"session_id\":\"denied-target\"}"}}]}),
            Value::Null,
        ),
        chunk(json!({}), json!("tool_calls")),
        "[DONE]".into(),
    ]
}

fn text_frames() -> Vec<String> {
    vec![
        chunk(
            json!({"reasoning_content":"private reasoning", "content":"Hello "}),
            Value::Null,
        ),
        chunk(json!({"content":"world"}), Value::Null),
        chunk(json!({}), json!("stop")),
        "[DONE]".into(),
    ]
}

fn shipped_handoff_frames() -> Vec<String> {
    vec![
        chunk(
            json!({"tool_calls":[{"index":0,"id":"shipped-handoff","type":"function",
                "function":{"name":"atlas_session_handoff", "arguments":json!({
                    "prompt":"execute plan", "session_id":"shipped-atlas"
                }).to_string()}}]}),
            Value::Null,
        ),
        chunk(json!({}), json!("tool_calls")),
        "[DONE]".into(),
    ]
}

/// The tool call arrives on the steering task's first request; the follow-up
/// request (with the steered row as its last user message) answers as text.
fn steering_frames(first: bool) -> Vec<String> {
    if first {
        counted_tool_frames()
    } else {
        text_frames()
    }
}

/// Root round: plain text, so the steered row is owed after its TurnEnd.
/// Continuation round with the steered row: the counted bash_exec call the
/// confirmation middleware parks as a HITL approval. Post-approval round:
/// the distinct steered answer.
fn steering_approval_frames(requests: usize) -> Vec<String> {
    match requests {
        1 => text_frames(),
        2 => counted_tool_frames(),
        _ => vec![
            chunk(json!({"content":"Steered approved"}), Value::Null),
            chunk(json!({}), json!("stop")),
            "[DONE]".into(),
        ],
    }
}

/// Round one answers the root prompt as plain text, so its TurnEnd lands
/// while the steered row is still owed. The steered continuation round
/// (request two) is held with its own answer text so a recovered result can
/// be told apart from the root round's; later rounds behave like plain text.
fn steering_held_frames(requests: usize) -> Vec<String> {
    match requests {
        1 => text_frames(),
        2 => vec![
            chunk(json!({"content":"steered answer"}), Value::Null),
            chunk(json!({}), json!("stop")),
            "[DONE]".into(),
        ],
        _ => text_frames(),
    }
}
