use anyhow::Result;
use harnx_core::{execution_context::put_result_execution_context, instance::ServerScope};
use harnx_toolset::{
    ToolInputRequired, ToolInputResponse, ToolInvocation, ToolInvokeError, ToolSpec, Toolset,
};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::{ffi::OsString, sync::Arc, time::Duration};

pub struct Env(Vec<(&'static str, Option<OsString>)>);
impl Env {
    // qual:allow(complexity, unsafe) reason: "Nextest isolates test processes; environment variables configure NATS harness."
    pub fn set(values: &[(&'static str, &str)]) -> Self {
        let old = values
            .iter()
            .map(|(key, _)| (*key, std::env::var_os(key)))
            .collect();
        for (key, value) in values {
            unsafe { std::env::set_var(key, value) };
        }
        Self(old)
    }
}
impl Drop for Env {
    // qual:allow(complexity, unsafe) reason: "Restore environment variables in nextest-isolated process."
    fn drop(&mut self) {
        for (key, value) in &self.0 {
            unsafe {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            };
        }
    }
}

#[derive(Default)]
// qual:allow(srp, god_struct) reason: "One test probe owns both the effect counter and the response barrier so crash tests can stop between a tool effect and its reply."
pub struct Probe {
    pub calls: Mutex<Vec<(String, Value)>>,
    pub invocations: Mutex<Vec<(String, String)>>,
    pub block_reply: Mutex<Option<Arc<tokio::sync::Notify>>>,
    /// What the `challenge` tool saw per invocation: mode, the challenge
    /// responses delivered beside the arguments, and the request state.
    pub challenge_seen: Mutex<Vec<Value>>,
}

impl Drop for Probe {
    fn drop(&mut self) {
        if let Some(notify) = self.block_reply.lock().take() {
            notify.notify_waiters();
        }
    }
}

impl Probe {
    pub fn assert_single_call(&self, expected_tool: &str) {
        let calls = self.calls.lock().clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, expected_tool);
    }

    pub fn call_count(&self, tool: &str) -> usize {
        self.calls.lock().iter().filter(|(t, _)| t == tool).count()
    }

    pub fn total_calls(&self) -> usize {
        self.calls.lock().len()
    }

    #[cfg(feature = "fault-injection")]
    pub fn block_reply_on(&self) -> Arc<tokio::sync::Notify> {
        let notify = Arc::new(tokio::sync::Notify::new());
        *self.block_reply.lock() = Some(notify.clone());
        notify
    }

    #[cfg(feature = "fault-injection")]
    pub fn unblock_reply(&self) {
        if let Some(notify) = self.block_reply.lock().take() {
            notify.notify_waiters();
        }
    }
}

pub struct ProbeToolset(pub Arc<Probe>);

#[async_trait::async_trait]
impl Toolset for ProbeToolset {
    fn name(&self) -> &str {
        "probe"
    }

    fn tools(&self) -> Vec<ToolSpec> {
        vec![
            ToolSpec {
                name: "run".into(),
                description: "Probe run".into(),
                input_schema: json!({"type": "object"}),
                idempotent_hint: true,
                read_only_hint: false,
                timeout_secs: None,
                meta: None,
                cancellation_guarantee: Default::default(),
            },
            ToolSpec {
                name: "aux".into(),
                description: "Probe aux".into(),
                input_schema: json!({"type": "object"}),
                idempotent_hint: true,
                read_only_hint: false,
                timeout_secs: None,
                meta: None,
                cancellation_guarantee: Default::default(),
            },
            ToolSpec {
                name: "challenge".into(),
                description: "Challenges the user for input before finishing".into(),
                input_schema: json!({"type": "object"}),
                idempotent_hint: true,
                read_only_hint: false,
                timeout_secs: None,
                meta: None,
                cancellation_guarantee: Default::default(),
            }
            .with_input_required(),
        ]
    }

    async fn invoke(
        &self,
        tool: &str,
        args: Value,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Value, ToolInvokeError> {
        self.0
            .calls
            .lock()
            .push((format!("probe_{tool}"), args.clone()));
        let blocker = self.0.block_reply.lock().clone();
        if let Some(notify) = blocker {
            notify.notified().await;
        }
        let mut out = json!({
            "value": args.get("value").cloned().unwrap_or(json!("ok")),
            "branch": args.get("branch").cloned().unwrap_or(json!("none")),
        });
        if args.get("observe") == Some(&json!(true)) {
            let path = std::path::Path::new("/workspace/probe_b");
            let obs =
                harnx_core::execution_context::ExecutionContextObservation::observe(path, path);
            put_result_execution_context(&mut out, serde_json::to_value(obs).unwrap());
        }
        Ok(out)
    }

    async fn invoke_with_context(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Value, ToolInvokeError> {
        let call_id = invocation.context.call_id.clone();
        let tool = invocation.tool.clone();
        self.0.invocations.lock().push((tool.clone(), call_id));
        if tool == "challenge" {
            self.invoke_challenge(invocation).await
        } else {
            self.invoke(&tool, invocation.args, invocation.cancel).await
        }
    }
}

impl ProbeToolset {
    /// The `challenge` tool exercises T4.3: it suspends by returning
    /// `input_required` and reads the answers from the invocation context
    /// on the rerun. `mode` selects the challenge shape:
    /// - `confirm`: one confirm challenge, answered once.
    /// - `a2ui`: one A2UI form challenge, answered with form data.
    /// - `double`: challenges twice; each rerun must see only the latest
    ///   challenge's responses and state.
    /// - `loop`: challenges on every invocation, forever; the runtime's
    ///   round cap is the only thing that ends it.
    async fn invoke_challenge(&self, invocation: ToolInvocation) -> Result<Value, ToolInvokeError> {
        let args = invocation.args.clone();
        // Record exactly what the model authored: challenge answers must
        // arrive beside the arguments, never merged into them.
        self.0
            .calls
            .lock()
            .push(("probe_challenge".into(), args.clone()));
        let mode = args
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("confirm")
            .to_string();
        let responses = invocation.context.input_responses.clone();
        let state = invocation.context.request_state.clone();
        let answered: serde_json::Map<String, Value> = responses
            .iter()
            .map(|(key, response): (_, &ToolInputResponse)| {
                (
                    key.clone(),
                    json!({
                        "action": response.action,
                        "data": response.data,
                        "note": response.note,
                    }),
                )
            })
            .collect();
        self.0.challenge_seen.lock().push(json!({
            "mode": mode,
            "responses": answered,
            "state": state,
        }));
        // `loop`: never satisfied; every rerun challenges again with a fresh
        // key and state. Used by the round-cap case to prove the runtime cuts
        // off a tool that would otherwise suspend forever.
        if mode == "loop" {
            let round = self.0.challenge_seen.lock().len();
            return Err(ToolInvokeError::InputRequired(Box::new(
                ToolInputRequired::confirm(
                    format!("round-{round}"),
                    format!("Loop round {round}: proceed?"),
                    json!({"round": round, "token": format!("probe-state-{round}")}),
                ),
            )));
        }
        if responses.is_empty() {
            return Err(ToolInvokeError::InputRequired(Box::new(
                match mode.as_str() {
                    "a2ui" => ToolInputRequired::a2ui(
                        "form-key",
                        a2ui_surface(),
                        "fill the probe form",
                        json!({"round": 1, "token": "probe-state-1"}),
                    ),
                    "double" => ToolInputRequired::confirm(
                        "step-1",
                        "First: proceed?",
                        json!({"round": 1, "token": "probe-state-1"}),
                    ),
                    _ => ToolInputRequired::confirm(
                        "ask-key",
                        "Allow the probe to proceed?",
                        json!({"round": 1, "token": "probe-state-1"}),
                    ),
                },
            )));
        }
        if mode == "double" && responses.contains_key("step-1") && !responses.contains_key("step-2")
        {
            return Err(ToolInvokeError::InputRequired(Box::new(
                ToolInputRequired::confirm(
                    "step-2",
                    "Second: proceed?",
                    json!({"round": 2, "token": "probe-state-2"}),
                ),
            )));
        }
        Ok(json!({
            "value": args.get("value").cloned().unwrap_or(json!("ok")),
            "mode": mode,
            "responses": answered,
            "state": state,
        }))
    }
}

/// Valid A2UI v1.0 envelope, the same shape the middleware `interact`
/// surface validation accepts.
fn a2ui_surface() -> Value {
    serde_json::from_value(serde_json::json!({
        "version": "1.0",
        "createSurface": {
            "dataModel": {"choice": [], "name": ""},
            "components": [
                {"id": "root", "component": "Card", "child": "col"},
                {"id": "col", "component": "Column", "children": ["title", "choice", "name", "submit_label", "submit"]},
                {"id": "title", "component": "Text", "text": "Pick a choice"},
                {"id": "choice", "component": "ChoicePicker", "options": [{"label": "First", "value": "a"}, {"label": "Second", "value": "b"}], "value": {"@path": "/choice"}},
                {"id": "name", "component": "TextField", "label": "Name", "value": {"@path": "/name"}},
                {"id": "submit_label", "component": "Text", "text": "Submit"},
                {"id": "submit", "component": "Button", "child": "submit_label", "action": {"event": {"name": "submit"}}}
            ]
        }
    }))
    .expect("static a2ui surface must deserialize")
}

pub async fn serve_probe(
    url: &str,
    scope: &ServerScope,
    probe: Arc<Probe>,
) -> Result<tokio::task::JoinHandle<Result<()>>> {
    let url_str = url.to_string();
    let scope_copy = scope.clone();
    let task = tokio::spawn(async move {
        harnx_toolset_server::serve_over_nats(
            ProbeToolset(probe),
            scope_copy,
            &url_str,
            "test-token",
        )
        .await
    });
    let client = async_nats::ConnectOptions::new()
        .token("test-token".into())
        .connect(url)
        .await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(bucket) = async_nats::jetstream::new(client.clone())
                .get_key_value(harnx_toolset_server::TOOL_REGISTRY_BUCKET)
                .await
            {
                if bucket
                    .get(harnx_toolset_server::registration_key(scope, "____probe"))
                    .await?
                    .is_some()
                {
                    return Ok::<_, anyhow::Error>(());
                }
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    })
    .await??;
    Ok(task)
}
