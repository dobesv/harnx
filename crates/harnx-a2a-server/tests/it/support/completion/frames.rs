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
        let held = needs_hold(llm.script, counted_tool);
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

fn needs_hold(script: Script, counted_tool: bool) -> bool {
    match script {
        Script::Text => true,
        Script::CountedTool => !counted_tool,
        _ => false,
    }
}

fn needs_counted_tool(script: Script, body: &Value) -> bool {
    script == Script::CountedTool
        && body["messages"]
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

fn select_frames(llm: &Llm, first: bool, many: bool, counted_tool: bool) -> Vec<String> {
    if llm.script == Script::Overflow {
        return overflow_frames();
    }
    if many {
        return many_frames(llm.script);
    }
    if counted_tool {
        return counted_tool_frames();
    }
    if llm.script == Script::SameRoundTools && first {
        return same_round_frames();
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
