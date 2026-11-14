//! A wait that stops on new messages ends as soon as one arrives for its
//! session: one that lands while the wait runs, and one that lands while the
//! model is still deciding to wait.
use super::*;
use crate::worker::{append_admitted_fixture_user, start_tool_server};
use harnx_core::instance::ServerScope;
use harnx_core::tool::ToolResult;
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

const CHILD_START: &str = "wait for the parent to redirect this work";
const PARENT_REDIRECT: &str = "stop waiting and report back to the parent";
const CHILD_REPLY: &str = "child received the parent's redirect";

/// The parent side's next step after a tool result: one more tool call, or
/// the final reply.
enum ParentStep {
    Call(ToolCall),
    Done(String),
}

/// The parent side's tool-call chain: start the child, wait until the child's
/// wait has started before sending the redirect, then wait on the joined
/// handle and finish.
async fn parent_next_step(
    waits_started: &AsyncMutex<mpsc::UnboundedReceiver<()>>,
    observed: &mpsc::UnboundedSender<Value>,
    last_result: Option<ToolResult>,
) -> Result<ParentStep> {
    let Some(result) = last_result else {
        return Ok(ParentStep::Call(ToolCall::new(
            "helper_session_send".into(),
            json!({"message": CHILD_START}),
            Some("start-child".into()),
            None,
        )));
    };
    match result.call.id.as_deref() {
        Some("start-child") => {
            observed.send(json!({"initial_handle": result.output}))?;
            next(&mut *waits_started.lock().await).await?;
            let session_id = result.output["session_id"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("no child handle: {}", result.output))?;
            Ok(ParentStep::Call(ToolCall::new(
                "helper_session_send".into(),
                json!({"session_id": session_id, "message": PARENT_REDIRECT}),
                Some("wake-child".into()),
                None,
            )))
        }
        Some("wake-child") => {
            observed.send(json!({"joined_handle": result.output}))?;
            Ok(ParentStep::Call(ToolCall::new(
                "helper_session_wait".into(),
                json!({
                    "session_id": result.output["session_id"],
                    "prompt_id": result.output["prompt_id"],
                    "forever": true,
                }),
                Some("observe-child".into()),
                None,
            )))
        }
        Some("observe-child") => {
            observed.send(json!({"parent_wait": result.output}))?;
            Ok(ParentStep::Done("parent done".into()))
        }
        _ => anyhow::bail!("unexpected parent tool result: {result:?}"),
    }
}

/// Both sides use the worker's real delegation tools. The parent sends its
/// redirect only after the child's tool reports that its wait has started.
fn parent_wakes_child_call_fn(
    waits_started: mpsc::UnboundedReceiver<()>,
    observed: mpsc::UnboundedSender<Value>,
) -> harnx_runtime::agent_loop::AgentCallFn {
    let waits_started = Arc::new(AsyncMutex::new(waits_started));
    Arc::new(move |input, config, _abort| {
        let user_texts = harnx_runtime::config::input::build_messages(input, config)
            .unwrap()
            .iter()
            .filter(|message| message.role.is_user())
            .map(|message| message.content.to_text())
            .collect::<Vec<_>>();
        let is_child = user_texts.iter().any(|text| text.contains(CHILD_START));
        let last_result = input
            .tool_calls
            .as_ref()
            .and_then(|calls| calls.tool_results.last())
            .cloned();
        let waits_started = Arc::clone(&waits_started);
        let observed = observed.clone();
        Box::pin(async move {
            if is_child {
                let Some(result) = last_result else {
                    return wait_call("child-wait");
                };
                observed.send(json!({
                    "child_request": user_texts,
                    "child_wait": result.output,
                }))?;
                return Ok((CHILD_REPLY.into(), None, vec![], Default::default()));
            }
            let call = match parent_next_step(&waits_started, &observed, last_result).await? {
                ParentStep::Call(call) => call,
                ParentStep::Done(text) => return Ok((text, None, vec![], Default::default())),
            };
            Ok((String::new(), None, vec![call], Default::default()))
        })
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parent_session_send_wakes_waiting_child_and_waits_for_joined_response() -> Result<()> {
    require_nextest();
    let server = spawn_nats_server()
        .await?
        .ok_or_else(|| anyhow::anyhow!("nats-server required for the busy-child wake test"))?;
    let client = async_nats::connect(server.url()).await?;
    let js = async_nats::jetstream::new(client.clone());
    let scope = ServerScope::new();
    let _environment = EnvGuard::tool_server_environment(&scope, server.url());
    let config_dir = tempfile::tempdir()?;
    std::fs::create_dir(config_dir.path().join("agents"))?;
    std::fs::write(
        config_dir.path().join("agents/helper.md"),
        "---\nuse_tools: steer_wait\n---\nWait for the parent's redirect.\n",
    )?;
    let _config_dir = EnvGuard::set("HARNX_CONFIG_DIR", config_dir.path().to_str().unwrap());
    let (started, waits_started) = mpsc::unbounded_channel();
    let (outcomes, mut wait_outcomes) = mpsc::unbounded_channel();
    let _tool_server = start_tool_server(
        &js,
        client,
        scope,
        Arc::new(SteerWait { started, outcomes }),
    )
    .await?;
    let (observed, mut observations) = mpsc::unbounded_channel();
    let _daemon = AbortOnDropHandle::new(tokio::spawn(run_worker_daemon(
        local_nats_runtime_config(server.url()),
        WorkerDaemonConfig::new("local", "worker-parent-wakes-child"),
        Some(parent_wakes_child_call_fn(waits_started, observed)),
        None,
    )));

    let session_id = "parent-wakes-child";
    seed_session_with_tool(&js, session_id, "*").await?;
    let log = NatsSessionLog::new_with_replicas(js.clone(), storage_key(session_id), 1);
    append_admitted_fixture_user(&log, "parent-start", "delegate work, then redirect it").await?;
    activate_session(&js, session_id).await?;

    let outcome = next(&mut wait_outcomes).await;
    if outcome.is_err() {
        eprintln!("parent log: {:#?}", log.load_events_async().await?);
    }
    assert_stopped_early(
        outcome?,
        "the parent's session_send wakes the busy child's wait",
    );
    assert_joined_response(&js, &mut observations).await
}

async fn assert_joined_response(
    js: &async_nats::jetstream::Context,
    observations: &mut mpsc::UnboundedReceiver<Value>,
) -> Result<()> {
    // Receipt and child model request may arrive in either order.
    let mut initial = None;
    let mut receipt = None;
    let mut child_request = None;
    let mut parent_wait = None;
    for _ in 0..4 {
        let observation = next(observations).await?;
        if let Some(value) = observation.get("initial_handle") {
            initial = Some(value.clone());
        } else if let Some(value) = observation.get("joined_handle") {
            receipt = Some(value.clone());
        } else if observation.get("child_request").is_some() {
            child_request = Some(observation);
        } else {
            parent_wait = observation.get("parent_wait").cloned();
        }
    }
    let receipt = receipt.expect("parent received the joined dispatch handle");
    let initial = initial.expect("parent received the original dispatch handle");
    assert_eq!(receipt["session_id"], initial["session_id"]);
    assert_ne!(receipt["prompt_id"], initial["prompt_id"]);
    let request = child_request.expect("child made its next model request");
    assert_eq!(request["child_wait"]["stopped_by"], "new_message");
    let texts: Vec<String> = serde_json::from_value(request["child_request"].clone())?;
    assert_eq!(occurrences(&texts, PARENT_REDIRECT), 1);
    let result = parent_wait.expect("parent session_wait returned");
    assert_eq!(result["status"], "completed", "{result}");
    assert_eq!(result["terminal"], true, "{result}");
    assert!(result.to_string().contains(CHILD_REPLY), "{result}");

    let child_storage = harnx_core::session_identity::session_key(
        Some("helper"),
        receipt["session_id"].as_str().unwrap(),
    );
    let metadata = SessionMetadataStore::ensure(js, 1).await?;
    let admission = metadata
        .prompt_admission(&child_storage, receipt["prompt_id"].as_str().unwrap())
        .await?
        .expect("joined prompt has an execution admission");
    assert_eq!(
        admission.invocation_id.as_str(),
        initial["prompt_id"].as_str().unwrap()
    );
    Ok(())
}
