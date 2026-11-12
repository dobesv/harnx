//! A wait that stops on new messages ends as soon as one arrives for its
//! session: one that lands while the wait runs, and one that lands while the
//! model is still deciding to wait.
use super::*;
use crate::worker::{append_admitted_fixture_user, start_tool_server};
use harnx_core::instance::ServerScope;
use harnx_runtime::nats_tool_provider::NatsInFlightCalls;
use harnx_toolset::{CancellationGuarantee, ToolInvocation, ToolInvokeError, ToolSpec, Toolset};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;

/// Each wait's own limit, which no wait in this test may reach.
const WAIT_LIMIT: Duration = Duration::from_secs(30);
/// Generous for a degraded CI runner, and still well short of `WAIT_LIMIT`.
const EARLY_STOP: Duration = Duration::from_secs(20);

/// How one wait ended and how long it ran.
#[derive(Debug)]
struct WaitOutcome {
    stopped_by: &'static str,
    elapsed: Duration,
}

/// `steer_wait`: waits for a new message, up to `WAIT_LIMIT`.
struct SteerWait {
    started: mpsc::UnboundedSender<()>,
    outcomes: mpsc::UnboundedSender<WaitOutcome>,
}

#[async_trait::async_trait]
impl Toolset for SteerWait {
    fn name(&self) -> &str {
        "steer"
    }

    fn tools(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "wait".into(),
            description: "Wait until a new message arrives".into(),
            input_schema: json!({"type": "object"}),
            idempotent_hint: false,
            read_only_hint: true,
            cancellation_guarantee: CancellationGuarantee::Cooperative,
            timeout_secs: Some(0),
            meta: None,
        }
        .with_stop_on_new_message()]
    }

    async fn invoke(
        &self,
        _tool: &str,
        _args: Value,
        _cancel: CancellationToken,
    ) -> std::result::Result<Value, ToolInvokeError> {
        Err(ToolInvokeError::Recoverable(
            "steer_wait needs its invocation context".into(),
        ))
    }

    async fn invoke_with_context(
        &self,
        invocation: ToolInvocation,
    ) -> std::result::Result<Value, ToolInvokeError> {
        let started = std::time::Instant::now();
        let _ = self.started.send(());
        let stopped_by = tokio::select! {
            _ = invocation.context.new_message.cancelled() => "new_message",
            _ = tokio::time::sleep(WAIT_LIMIT) => "limit",
        };
        let elapsed = started.elapsed();
        let _ = self.outcomes.send(WaitOutcome {
            stopped_by,
            elapsed,
        });
        Ok(json!({"status": "running", "stopped_by": stopped_by}))
    }
}

/// The model's side: the user texts each request carried, and a gate that
/// holds the second request until the test has queued a message.
#[derive(Default)]
struct SteeredModel {
    requests: std::sync::Mutex<Vec<Vec<String>>>,
    second_request: Notify,
    release_second: Notify,
}

impl SteeredModel {
    fn requests(&self) -> Vec<Vec<String>> {
        self.requests.lock().unwrap().clone()
    }
}

fn wait_call(id: &str) -> Result<(String, Option<String>, Vec<ToolCall>, CompletionTokenUsage)> {
    let call = ToolCall::new("steer_wait".into(), json!({}), Some(id.into()), None);
    Ok((
        String::new(),
        None,
        vec![call],
        CompletionTokenUsage::default(),
    ))
}

/// Wait once, wait again once the test has queued a message, then finish.
fn steered_call_fn(model: Arc<SteeredModel>) -> harnx_runtime::agent_loop::AgentCallFn {
    Arc::new(move |input, config, _abort| {
        let model = Arc::clone(&model);
        let user_texts = harnx_runtime::config::input::build_messages(input, config)
            .unwrap()
            .iter()
            .filter(|message| message.role.is_user())
            .map(|message| message.content.to_text())
            .collect::<Vec<_>>();
        let round = {
            let mut requests = model.requests.lock().unwrap();
            requests.push(user_texts);
            requests.len()
        };
        Box::pin(async move {
            match round {
                1 => wait_call("wait-1"),
                2 => {
                    model.second_request.notify_one();
                    model.release_second.notified().await;
                    wait_call("wait-2")
                }
                _ => Ok(("done".into(), None, vec![], CompletionTokenUsage::default())),
            }
        })
    })
}

async fn seed_session_with_tool(
    js: &async_nats::jetstream::Context,
    session_id: &str,
    tool: &str,
) -> Result<()> {
    let metadata = SessionMetadata::new(
        session_id,
        SessionInitializer::inline(
            "",
            Default::default(),
            SessionOverrides {
                use_tools: Some(vec![tool.into()]),
                ..Default::default()
            },
        ),
    );
    SessionMetadataStore::ensure(js, 1)
        .await?
        .create(&metadata)
        .await?;
    Ok(())
}

async fn next<T>(receiver: &mut mpsc::UnboundedReceiver<T>) -> Result<T> {
    tokio::time::timeout(CI_SAFE_TIMEOUT, receiver.recv())
        .await?
        .ok_or_else(|| anyhow::anyhow!("the tool server stopped"))
}

fn assert_stopped_early(outcome: WaitOutcome, why: &str) {
    assert_eq!(outcome.stopped_by, "new_message", "{why}");
    assert!(
        outcome.elapsed < EARLY_STOP,
        "{why}: the wait ran {:?}",
        outcome.elapsed
    );
}

fn occurrences(texts: &[String], needle: &str) -> usize {
    texts.iter().filter(|text| text.contains(needle)).count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn new_message_ends_an_opted_in_wait_early() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        eprintln!("Skipping test: nats-server not available");
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let js = async_nats::jetstream::new(client.clone());
    let scope = ServerScope::new();
    let _environment = EnvGuard::tool_server_environment(&scope, server.url());
    let (started, mut waits_started) = mpsc::unbounded_channel();
    let (outcomes, mut wait_outcomes) = mpsc::unbounded_channel();
    let tool = Arc::new(SteerWait { started, outcomes });
    let _tool_server = start_tool_server(&js, client, scope.clone(), tool).await?;
    let model = Arc::new(SteeredModel::default());
    let _daemon = AbortOnDropHandle::new(tokio::spawn(run_worker_daemon(
        local_nats_runtime_config(server.url()),
        WorkerDaemonConfig::new("local", "worker-steered-wait"),
        Some(steered_call_fn(Arc::clone(&model))),
        None,
    )));

    let session_id = "steered-wait";
    seed_session_with_tool(&js, session_id, "steer_wait").await?;
    let log = NatsSessionLog::new_with_replicas(js.clone(), storage_key(session_id), 1);
    append_admitted_fixture_user(&log, "user-1", "wait for me").await?;
    activate_session(&js, session_id).await?;

    next(&mut waits_started).await?;
    append_admitted_fixture_user(&log, "user-2", "steer one").await?;
    let first = next(&mut wait_outcomes).await?;
    assert_stopped_early(first, "a message that lands during the wait ends it");

    // Queue the next message while the model is still deciding to wait, and
    // let it decide only once the worker has seen that message.
    tokio::time::timeout(CI_SAFE_TIMEOUT, model.second_request.notified()).await?;
    let in_flight = NatsInFlightCalls::for_instance(&scope);
    let key = storage_key(session_id);
    assert!(
        !in_flight.message_pending(&key),
        "round 1's injection took in steer one"
    );
    append_admitted_fixture_user(&log, "user-3", "steer two").await?;
    wait_until(CI_SAFE_TIMEOUT, || in_flight.message_pending(&key)).await?;
    model.release_second.notify_one();
    let second = next(&mut wait_outcomes).await?;
    assert_stopped_early(second, "a message queued before the wait ends it at once");

    wait_until(CI_SAFE_TIMEOUT, || model.requests().len() >= 3).await?;
    let requests = model.requests();
    assert_eq!(
        [
            occurrences(&requests[1], "steer one"),
            occurrences(&requests[2], "steer two")
        ],
        [1, 1],
        "each request after a wait carries the new message exactly once"
    );
    Ok(())
}
