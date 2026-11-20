mod cases;

use super::fixture::{self, Probe};
use crate::common;
use anyhow::{Context, Result};
use harnx_core::{
    abort::create_abort_signal,
    instance::ServerScope,
    message::{Message, MessageContent},
    tool::ToolCall,
};
use harnx_runtime::{
    agent_loop::{run_agent_loop, AgentCallFn, AgentLoopContext},
    client::CompletionTokenUsage,
    config::GlobalConfig,
};
use harnx_toolset::{ToolInvokeError, ToolSpec, Toolset};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::{
    ffi::OsString,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

struct ProbeToolset(Arc<Probe>);
#[async_trait::async_trait]
impl Toolset for ProbeToolset {
    fn name(&self) -> &str {
        "probe"
    }
    fn tools(&self) -> Vec<ToolSpec> {
        ["run", "aux"]
            .into_iter()
            .map(|name| ToolSpec {
                name: name.into(),
                description: "Probe".into(),
                input_schema: json!({"type":"object"}),
                idempotent_hint: true,
                read_only_hint: true,
                timeout_secs: None,
                meta: None,
                cancellation_guarantee: Default::default(),
            })
            .collect()
    }
    async fn invoke(
        &self,
        tool: &str,
        args: Value,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> std::result::Result<Value, ToolInvokeError> {
        self.0
            .invoke(&format!("probe_{tool}"), args)
            .await
            .map(|output| output.value)
            .map_err(|error| match error {
                harnx_core::tool::ToolError::Fatal(error) => {
                    ToolInvokeError::Fatal(error.to_string())
                }
                harnx_core::tool::ToolError::Recoverable(error) => {
                    ToolInvokeError::Recoverable(error.to_string())
                }
            })
    }
}

struct Env(Vec<(&'static str, Option<OsString>)>);
impl Env {
    // qual:allow(complexity, unsafe) reason: "Nextest isolates this process; broker/worker configuration reads environment variables."
    fn set(values: &[(&'static str, &str)]) -> Self {
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
    // qual:allow(complexity, unsafe) reason: "Restore environment variables in this nextest-isolated process."
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

fn mock_model() -> (AgentCallFn, Arc<Mutex<Vec<Message>>>) {
    mock_model_for(fixture::call(json!({"value":"original"})))
}

fn mock_model_for(call: ToolCall) -> (AgentCallFn, Arc<Mutex<Vec<Message>>>) {
    let turn = Arc::new(AtomicUsize::new(0));
    let messages = Arc::new(Mutex::new(vec![]));
    let seen = messages.clone();
    let model: AgentCallFn = Arc::new(move |input, config, _| {
        let first = turn.fetch_add(1, Ordering::SeqCst) == 0;
        if !first {
            *seen.lock() = harnx_runtime::config::input::build_messages(input, config).unwrap();
        }
        let call = call.clone();
        Box::pin(async move {
            Ok((
                "done".into(),
                None,
                if first { vec![call] } else { vec![] },
                CompletionTokenUsage::default(),
            ))
        })
    });
    (model, messages)
}

fn context(config: GlobalConfig, instance_id: ServerScope, model: AgentCallFn) -> AgentLoopContext {
    AgentLoopContext {
        config,
        instance_id,
        abort_signal: create_abort_signal(),
        token_budget: None,
        usage_at_start: Default::default(),
        call_fn: Some(model),
        on_tool_round: None,
        on_text_response: None,
        initial_with_embeddings: false,
        initial_resume_count: 0,
        max_resume: None,
        prepare_handoff_fn: None,

        pending_async_context: None,
        surface_notes: None,
        working_dir: None,
        tool_loop_guard: Default::default(),
    }
}

fn assert_model_result(messages: &[Message]) {
    let results = messages
        .iter()
        .filter_map(|message| match &message.content {
            MessageContent::ToolCalls(calls) => Some(&calls.tool_results),
            _ => None,
        })
        .flatten()
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 1, "{messages:?}");
    assert_eq!(results[0].output["value"], "changed-wrapped");
}

async fn serve_probe(
    url: &str,
    scope: &ServerScope,
    probe: Arc<Probe>,
) -> Result<tokio::task::JoinHandle<Result<()>>> {
    let url = url.to_string();
    let scope_copy = scope.clone();
    let task = tokio::spawn(async move {
        harnx_toolset_server::serve_over_nats(ProbeToolset(probe), scope_copy, &url, "test-token")
            .await
    });
    let client = async_nats::ConnectOptions::new()
        .token("test-token".into())
        .connect(std::env::var("HARNX_NATS_URL")?)
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
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    Ok(task)
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_bash_and_python_rewrites_reach_mock_model() -> Result<()> {
    let server = common::spawn_nats_server()
        .await?
        .context("nats-server required for middleware acceptance")?;
    let scope = ServerScope::new();
    let _env = Env::set(&[
        ("HARNX_NATS_URL", server.url()),
        ("HARNX_NATS_TOKEN", "test-token"),
    ]);
    let probe = Arc::new(Probe::default());
    let task = serve_probe(server.url(), &scope, probe.clone()).await?;
    let _task = task.abort_handle();
    for entry in [
        fixture::bash(
            r#"r=$(next '{"value":"changed"}'); respond "$(echo "$r" | jq '.output.value += "-wrapped"')""#,
        ),
        fixture::python(
            "r = call_next({'value':'changed'})\nr['output']['value'] += '-wrapped'\nrespond(r)",
        ),
    ] {
        let config = fixture::config(vec![entry]);
        let (model, seen) = mock_model();
        let ctx = context(config.clone(), scope.clone(), model);
        let input = harnx_runtime::config::input::from_str(&config, "run probe", None);
        run_agent_loop(&ctx, input).await?;
        assert_model_result(&seen.lock());
    }
    assert_eq!(probe.calls.lock().len(), 2);
    assert!(probe
        .calls
        .lock()
        .iter()
        .all(|(_, args)| args["value"] == "changed"));
    task.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_runs_in_worker_without_manage_servers() -> Result<()> {
    use futures_util::StreamExt;
    use harnx_runtime::{
        config::{Config, ConfigLock},
        nats_session::{NatsSession, NatsSessionConfig},
        nats_session_metadata::SessionInitializer,
        nats_worker::{run_worker_daemon, WorkerDaemonConfig},
    };
    let server = common::spawn_nats_server()
        .await?
        .context("nats-server required for middleware acceptance")?;
    let root = tempfile::tempdir()?;
    let config_dir = root.path().join("config");
    for dir in ["agents", "clients", "nats_servers"] {
        std::fs::create_dir_all(config_dir.join(dir))?;
    }
    std::fs::write(config_dir.join("config.yaml"), "model: openai:test-model\n")?;
    std::fs::write(config_dir.join("clients/openai.yaml"),"type: openai\napi_key: sk-test\nmodels:\n  - name: test-model\n    type: chat\n    max_input_tokens: 4096\n")?;
    std::fs::write(
        config_dir.join("nats_servers/local.yaml"),
        format!("url: {}\n", server.url()),
    )?;
    std::fs::write(
        config_dir.join("agents/middleware-agent.md"),
        r#"---
model: openai:test-model
use_tools: [probe_run]
middleware:
- name: worker-rewrite
  match: '^probe_run$'
  python: |
    r = call_next({'value':'changed'})
    r['output']['value'] += '-wrapped'
    respond(r)
---
Run probe
"#,
    )?;
    let _env = Env::set(&[
        ("HARNX_CONFIG_DIR", config_dir.to_str().unwrap()),
        ("HARNX_DATA_DIR", root.path().join("data").to_str().unwrap()),
        (
            "HARNX_STATE_DIR",
            root.path().join("state").to_str().unwrap(),
        ),
        ("HARNX_NATS_URL", server.url()),
        ("HARNX_NATS_TOKEN", "test-token"),
    ]);
    let config = Arc::new(ConfigLock::new(Config::load_from_file(
        &config_dir.join("config.yaml"),
    )?));
    let cfg = config.read().clone();
    let client = cfg.nats_client("local").await?;
    let js = cfg.nats_jetstream("local").await?;
    let scope = ServerScope::new();
    let _scope_env = Env::set(&[(harnx_core::instance::HARNX_SERVER_SCOPE, scope.as_str())]);
    let probe = Arc::new(Probe::default());
    let tool = serve_probe(server.url(), &scope, probe.clone()).await?;
    let (model, seen) = mock_model();
    let mut ready = client
        .subscribe(harnx_runtime::nats_worker::worker_ready_subject("local"))
        .await?;
    client.flush().await?;
    let worker_config = WorkerDaemonConfig::new("local", "middleware-unmanaged");
    assert!(!worker_config.manage_servers);
    let worker = tokio::spawn(run_worker_daemon(config, worker_config, Some(model), None));
    tokio::time::timeout(Duration::from_secs(30), ready.next())
        .await?
        .context("worker readiness closed")?;
    let session = NatsSession::new(
        NatsSessionConfig {
            cluster: "local".into(),
            initializer: SessionInitializer::named("middleware-agent", Default::default()),
            session_id: Some("middleware-unmanaged".into()),
            activation_route: harnx_runtime::SessionActivationRoute::ClusterShared,
        },
        client,
        js,
        create_abort_signal(),
    )
    .await?;
    tokio::time::timeout(
        Duration::from_secs(30),
        session.with_external_admission().run_turn(
            "run probe",
            Arc::new(harnx_core::event::NullSink),
            None,
        ),
    )
    .await??;
    assert_model_result(&seen.lock());
    assert_eq!(probe.calls.lock().len(), 1);
    worker.abort();
    tool.abort();
    Ok(())
}
