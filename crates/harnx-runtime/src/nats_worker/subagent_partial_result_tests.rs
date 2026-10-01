//! A sub-agent call names its child as the call's partial result as soon as
//! the child session is bound, so a parent whose call then fails still learns
//! which session ran.
use super::tests::{
    env_lock, registered_agent_provider, seed_remote_config, slow_prompt_call_fn,
    spawn_metis_worker_with_call_fn, spawn_test_nats, subagent_test_env, test_subagent_toolset,
};
use crate::nats_session_log::NatsSessionLog;
use harnx_core::abort::create_abort_signal;
use harnx_core::partial_result::partial_result_of;
use harnx_core::tool::{ToolError, ToolProvider};
use harnx_toolset::{
    PartialResultStore, ToolInvocation, ToolInvocationContext, ToolInvokeError, Toolset,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Keeps every partial result a call records, in order.
#[derive(Default)]
struct RecordedPartialResults(Mutex<Vec<Value>>);

#[async_trait::async_trait]
impl PartialResultStore for RecordedPartialResults {
    async fn record_partial_result(&self, value: Value) -> anyhow::Result<()> {
        self.0.lock().expect("partial results lock").push(value);
        Ok(())
    }
}

impl RecordedPartialResults {
    async fn first(&self) -> Value {
        loop {
            if let Some(value) = self
                .0
                .lock()
                .expect("partial results lock")
                .first()
                .cloned()
            {
                return value;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_child_is_the_partial_result_while_its_turn_runs() {
    let _env_guard = env_lock().await;
    let Some((url, mut nats, _store_dir)) = spawn_test_nats().await else {
        return;
    };
    let seeded = seed_remote_config(&url);
    let _env = subagent_test_env(&url, &seeded);
    // The child's turn outlasts the test, so the call can only end by abort.
    let daemon = spawn_metis_worker_with_call_fn(
        &url,
        slow_prompt_call_fn("never read", Duration::from_secs(120)),
    );
    let toolset = test_subagent_toolset(&url).await;
    let partial_results = Arc::new(RecordedPartialResults::default());
    let cancel = CancellationToken::new();
    let call = tokio::spawn({
        let partial_results = Arc::clone(&partial_results);
        let cancel = cancel.clone();
        async move {
            toolset
                .invoke_with_context(ToolInvocation {
                    tool: "session_prompt".into(),
                    args: json!({"message": "run until cancelled"}),
                    context: ToolInvocationContext {
                        call_id: "wire-partial".into(),
                        partial_result_store: Some(partial_results),
                        ..Default::default()
                    },
                    cancel,
                })
                .await
        }
    });

    let partial = tokio::time::timeout(Duration::from_secs(60), partial_results.first())
        .await
        .expect("the child is recorded once it is bound");
    assert!(
        !call.is_finished(),
        "the child is recorded while its turn still runs"
    );
    let child = partial["session_id"]
        .as_str()
        .expect("the partial result names the child");
    assert_eq!(
        partial["sub_agent"],
        json!({"agent": "metis", "session_id": child})
    );

    cancel.cancel();
    let outcome = call.await.expect("join the sub-agent call");
    assert!(
        matches!(outcome, Err(ToolInvokeError::Fatal(_))),
        "an aborted call fails: {outcome:?}"
    );

    daemon.abort();
    let _ = daemon.await;
    let _ = nats.kill();
    let _ = nats.wait();
}

/// The path a parent actually takes: its call goes out through the NATS tool
/// provider, the child's turn fails, and the failure that comes back carries
/// the child the call recorded on its journal row.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_sub_agent_call_returns_its_child_as_the_partial_result() {
    let _env_guard = env_lock().await;
    let Some((url, mut nats, _store_dir)) = spawn_test_nats().await else {
        return;
    };
    let mut seeded = seed_remote_config(&url);
    let _env = subagent_test_env(&url, &seeded);
    std::fs::write(
        seeded.config_dir().join("agents").join("metis.md"),
        "---\nmodel: test:test-model\n---\nConfigured metis agent\n",
    )
    .expect("write metis agent");
    // The provider journals the call under the parent's session, and reads
    // the partial result back from there.
    let parent_session =
        crate::config::session::new(&seeded.parent_config, "parent-failed-child", None)
            .expect("create parent session");
    let parent_session_id = parent_session.storage_key();
    seeded.parent_config.session = Some(parent_session);
    let daemon = spawn_metis_worker_with_call_fn(&url, failing_call_fn());
    let client = async_nats::connect(&url)
        .await
        .expect("connect parent observer");
    let jetstream = async_nats::jetstream::new(client);
    NatsSessionLog::new_with_replicas(jetstream.clone(), &parent_session_id, 1)
        .load_events_async()
        .await
        .expect("create parent transcript");
    let (_, provider, _) =
        registered_agent_provider(&jetstream, &seeded.parent_config, &["metis"], None).await;

    let abort = create_abort_signal();
    let call = provider.call_tool_with_id(
        "metis_session_prompt",
        json!({"message": "fail this turn"}),
        Some("parent-call"),
        &abort,
    );
    let error = match tokio::time::timeout(Duration::from_secs(120), call)
        .await
        .expect("the failed sub-agent call never answered")
    {
        Err(ToolError::Recoverable(error)) => error,
        Err(ToolError::Fatal(error)) => {
            panic!("a failed child turn must fail the call recoverably: {error:#}")
        }
        Ok(output) => panic!("a failed child turn must fail the call: {}", output.value),
    };

    let message = format!("{error:#}");
    assert!(
        message.contains("sub-agent turn failed"),
        "unexpected failure: {message}"
    );
    let partial = partial_result_of(&error)
        .unwrap_or_else(|| panic!("the failure carries no partial result: {message}"));
    assert_eq!(partial["sub_agent"]["agent"], "metis");
    let child = partial["session_id"]
        .as_str()
        .expect("the partial result names the child");
    assert_eq!(partial["sub_agent"]["session_id"], child);
    assert!(
        message.contains(child),
        "the failure names another child: {message}"
    );

    daemon.abort();
    let _ = daemon.await;
    let _ = nats.kill();
    let _ = nats.wait();
}

/// A model that fails every call, so each turn the child runs ends in an error.
fn failing_call_fn() -> crate::agent_loop::AgentCallFn {
    Arc::new(|_input, _config, _abort| Box::pin(async { anyhow::bail!("child model unavailable") }))
}
