use anyhow::Result;
use harnx_core::{execution_context::put_result_execution_context, instance::ServerScope};
use harnx_toolset::{ToolInvocation, ToolInvokeError, ToolSpec, Toolset};
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
        self.invoke(&tool, invocation.args, invocation.cancel).await
    }
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
