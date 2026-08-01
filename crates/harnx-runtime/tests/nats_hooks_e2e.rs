use anyhow::{Context, Result};
use async_trait::async_trait;
use harnx_core::event::{AgentEvent, AgentEventSink, NoticeEvent};
use harnx_core::hooks::{HookEvent, HookOutcome, HookPayload, HookResult, HookResultControl};
use harnx_core::instance::InstanceId;
use harnx_hookset::{FailPolicy, Hook, HookRegistration, HookSpec};
use harnx_hookset_server::{hook_registration_key, serve_over_nats, HOOK_REGISTRY_BUCKET};
use harnx_runtime::config::Config;
use harnx_runtime::nats_hook_provider::{HookDispatchMeta, NatsHookProvider};
use serde_json::{json, Value};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::sync::Mutex;

const TOKEN: &str = "nats-hooks-e2e-token";
const TEST_TOOL: &str = "test_tool";

struct NatsServerHandle {
    url: String,
    _store_dir: TempDir,
    child: Child,
}

impl Drop for NatsServerHandle {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct TestHook;

#[async_trait]
impl Hook for TestHook {
    fn name(&self) -> &str {
        "testhook"
    }

    fn hooks(&self) -> Vec<HookSpec> {
        vec![
            HookSpec {
                event: "PreToolUse".to_string(),
                matcher: Some(TEST_TOOL.to_string()),
                priority: 10,
                timeout_secs: Some(2),
                fail_policy: FailPolicy::Closed,
            },
            HookSpec {
                event: "PostToolUse".to_string(),
                matcher: Some(TEST_TOOL.to_string()),
                priority: 10,
                timeout_secs: Some(2),
                fail_policy: FailPolicy::Closed,
            },
        ]
    }

    async fn handle_hook(&self, payload: HookPayload) -> HookOutcome {
        match payload.hook_event {
            HookEvent::PreToolUse { mut tool_input, .. } => {
                if tool_input.get("deny_me") == Some(&Value::Bool(true)) {
                    return HookOutcome {
                        control: HookResultControl::Block {
                            reason: "denied by test hook".to_string(),
                        },
                        result: HookResult::default(),
                    };
                }
                tool_input
                    .as_object_mut()
                    .expect("test tool input must be an object")
                    .insert("nats_hook_seen".to_string(), Value::Bool(true));
                HookOutcome {
                    control: HookResultControl::Continue,
                    result: HookResult {
                        mutated_tool_input: Some(tool_input),
                        ..HookResult::default()
                    },
                }
            }
            HookEvent::PostToolUse { tool_input, .. }
                if tool_input.get("post_error") == Some(&Value::Bool(true)) =>
            {
                HookOutcome {
                    control: HookResultControl::Block {
                        reason: "post hook cannot block completed tool".to_string(),
                    },
                    result: HookResult::default(),
                }
            }
            HookEvent::PostToolUse { .. } => HookOutcome {
                control: HookResultControl::Continue,
                result: HookResult {
                    additional_context: Some("post-hook-context".to_string()),
                    ..HookResult::default()
                },
            },
            _ => HookOutcome {
                control: HookResultControl::Continue,
                result: HookResult::default(),
            },
        }
    }
}

#[derive(Clone, Default)]
struct CollectingSink {
    events: Arc<StdMutex<Vec<AgentEvent>>>,
}

impl AgentEventSink for CollectingSink {
    fn emit(&self, event: AgentEvent) {
        self.events
            .lock()
            .expect("event mutex poisoned")
            .push(event);
    }
}

impl CollectingSink {
    fn error_notices(&self) -> Vec<String> {
        self.events
            .lock()
            .expect("event mutex poisoned")
            .iter()
            .filter_map(|event| match event {
                AgentEvent::Notice(NoticeEvent::Error(message)) => Some(message.clone()),
                _ => None,
            })
            .collect()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn nats_hooks_deny_mutate_and_deliver_post_results() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let instance_id = InstanceId::new();
    let server_url = server.url.clone();
    let hook_instance_id = instance_id.clone();
    let hook_task = tokio::spawn(async move {
        serve_over_nats(TestHook, hook_instance_id, &server_url, TOKEN).await
    });
    let client = async_nats::ConnectOptions::new()
        .token(TOKEN.to_string())
        .connect(&server.url)
        .await?;
    wait_for_registration(&client, &instance_id).await?;

    // NatsHookProvider reserves __local__ for the worker's environment handoff.
    unsafe {
        std::env::set_var("HARNX_NATS_URL", &server.url);
        std::env::set_var("HARNX_NATS_TOKEN", TOKEN);
    }
    let provider = NatsHookProvider::discover(&Config::default(), instance_id).await?;
    let meta = HookDispatchMeta {
        session_id: "nats-hooks-e2e".to_string(),
        cwd: std::env::current_dir()?,
        resume_count: 0,
    };

    let denied = provider
        .dispatch_pre_tool_use(&pre_event(json!({"deny_me": true})), meta.clone())
        .await;
    assert_eq!(
        denied.control,
        HookResultControl::Block {
            reason: "denied by test hook".to_string()
        }
    );

    let mutated = provider
        .dispatch_pre_tool_use(&pre_event(json!({"value": 42})), meta.clone())
        .await;
    assert_eq!(mutated.control, HookResultControl::Continue);
    assert_eq!(
        mutated.result.mutated_tool_input,
        Some(json!({"value": 42, "nats_hook_seen": true}))
    );

    let pending = Arc::new(Mutex::new(None));
    provider.dispatch_post_tool_use(
        post_event(json!({"value": 42})),
        Some(Arc::clone(&pending)),
        meta.clone(),
    );
    wait_for_pending_context(&pending, "post-hook-context").await?;

    let sink = CollectingSink::default();
    harnx_core::sink::clear_agent_event_sink();
    harnx_core::sink::install_agent_event_sink(Arc::new(sink.clone()));
    provider.dispatch_post_tool_use(post_event(json!({"post_error": true})), Some(pending), meta);
    let errors = wait_for_error_notice(&sink).await?;
    assert!(
        errors.iter().any(|message| {
            message.contains("testhook hook returned Block")
                && message.contains("post hook cannot block completed tool")
        }),
        "expected post-hook Block Error Notice, got {errors:?}"
    );
    harnx_core::sink::clear_agent_event_sink();

    hook_task.abort();
    drop(server);
    Ok(())
}

fn pre_event(tool_input: Value) -> HookEvent {
    HookEvent::PreToolUse {
        tool_name: TEST_TOOL.to_string(),
        tool_input,
        tool_use_id: "pre-tool-use-id".to_string(),
    }
}

fn post_event(tool_input: Value) -> HookEvent {
    HookEvent::PostToolUse {
        tool_name: TEST_TOOL.to_string(),
        tool_input,
        tool_response: json!({"ok": true}),
        tool_use_id: "post-tool-use-id".to_string(),
    }
}

async fn wait_for_registration(
    client: &async_nats::Client,
    instance_id: &InstanceId,
) -> Result<HookRegistration> {
    let jetstream = async_nats::jetstream::new(client.clone());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(store) = jetstream.get_key_value(HOOK_REGISTRY_BUCKET).await {
            let key = hook_registration_key(instance_id, "testhook");
            if let Some(value) = store.get(&key).await? {
                return serde_json::from_slice(&value).context("decode hook registration");
            }
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for test hook registration");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_for_pending_context(
    pending: &Arc<Mutex<Option<String>>>,
    expected: &str,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if pending.lock().await.as_deref() == Some(expected) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "timed out waiting for pending hook context; got {:?}",
                pending.lock().await.as_deref()
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_for_error_notice(sink: &CollectingSink) -> Result<Vec<String>> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let errors = sink.error_notices();
        if !errors.is_empty() {
            return Ok(errors);
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for post-hook Error Notice");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn spawn_nats_server() -> Result<Option<NatsServerHandle>> {
    let Some(binary) = nats_server_binary() else {
        eprintln!("skipping NATS integration test: nats-server binary not found");
        return Ok(None);
    };

    const MAX_START_ATTEMPTS: usize = 5;
    for attempt in 1..=MAX_START_ATTEMPTS {
        let listener = TcpListener::bind("127.0.0.1:0").context("allocate NATS test port")?;
        let port = listener.local_addr()?.port();
        drop(listener);
        let store_dir = tempfile::tempdir().context("create NATS test store")?;
        let mut child = Command::new(&binary)
            .arg("-js")
            .arg("-sd")
            .arg(store_dir.path())
            .arg("-p")
            .arg(port.to_string())
            .arg("--auth")
            .arg(TOKEN)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("spawn nats-server")?;
        let url = format!("nats://127.0.0.1:{port}");

        match wait_for_nats_ready(&mut child, &url).await {
            Ok(()) => {
                return Ok(Some(NatsServerHandle {
                    url,
                    _store_dir: store_dir,
                    child,
                }));
            }
            Err(error) => {
                let exited_during_startup = child.try_wait()?.is_some();
                let _ = child.kill();
                let _ = child.wait();
                if exited_during_startup && attempt < MAX_START_ATTEMPTS {
                    eprintln!(
                        "nats-server exited during startup attempt {attempt}; retrying with a new port: {error:#}"
                    );
                    continue;
                }
                return Err(error).context(format!(
                    "start nats-server after {attempt} attempt{}",
                    if attempt == 1 { "" } else { "s" }
                ));
            }
        }
    }
    unreachable!("NATS startup loop always returns")
}

async fn wait_for_nats_ready(child: &mut Child, url: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let result = async_nats::ConnectOptions::new()
            .token(TOKEN.to_string())
            .connect(url)
            .await;
        match result {
            Ok(client) => return client.flush().await.context("flush NATS test client"),
            Err(error) if child.try_wait()?.is_some() => {
                anyhow::bail!("nats-server exited during startup: {error}");
            }
            Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => return Err(error).context("wait for nats-server readiness"),
        }
    }
}

fn nats_server_binary() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("NATS_SERVER_BIN") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Some(path);
        }
    }
    which::which("nats-server").ok()
}
