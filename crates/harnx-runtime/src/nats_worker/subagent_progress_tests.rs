use super::tests::{
    env_lock, fixed_prompt_call_fn, seed_remote_config, slow_prompt_call_fn,
    spawn_metis_worker_with_call_fn, spawn_test_nats, subagent_test_env, test_subagent_toolset,
};
use futures_util::StreamExt;
use harnx_core::event::{AgentEvent, SubAgentProgress, SubAgentProgressStatus, TurnEvent};
use harnx_toolset::Toolset;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

async fn progress_toolset(url: &str) -> Arc<super::subagent_toolset::SubagentToolset> {
    let client = async_nats::connect(url)
        .await
        .expect("connect sub-agent toolset to test nats");
    Arc::new(
        super::subagent_toolset::SubagentToolset::new(
            "metis",
            super::subagent_toolset::SubagentSessionRoute::new(
                "local",
                super::SessionActivationRoute::ClusterShared,
            ),
            client.clone(),
            async_nats::jetstream::new(client),
        )
        .with_progress_heartbeat(Duration::from_millis(50)),
    )
}

async fn observe_progress(
    events: &mut async_nats::Subscriber,
) -> (Vec<SubAgentProgress>, SubAgentProgress) {
    let mut invocation_id = None;
    let mut running = Vec::new();
    loop {
        let message = events.next().await.expect("parent events remain open");
        let envelope = crate::nats_event_sink::AdvisoryEnvelope::from_bytes(&message.payload)
            .expect("decode progress advisory");
        let AgentEvent::SubAgent { event, .. } = envelope.event else {
            continue;
        };
        match *event {
            AgentEvent::Turn(TurnEvent::SubAgentStarted {
                invocation_id: Some(id),
                ..
            }) => assert!(invocation_id.replace(id).is_none(), "start is emitted once"),
            AgentEvent::Turn(TurnEvent::SubAgentProgress(progress)) => {
                assert_eq!(
                    Some(progress.invocation_id.as_str()),
                    invocation_id.as_deref()
                );
                if progress.status == SubAgentProgressStatus::Running {
                    running.push(progress);
                } else {
                    return (running, progress);
                }
            }
            _ => {}
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn orders_start_heartbeats_terminal_and_durable_summary() {
    let _env_guard = env_lock().await;
    let Some((url, mut nats, _store_dir)) = spawn_test_nats().await else {
        return;
    };
    let seeded = seed_remote_config(&url);
    let _env = subagent_test_env(&url, &seeded);
    let daemon = spawn_metis_worker_with_call_fn(
        &url,
        slow_prompt_call_fn("progress child response", Duration::from_millis(250)),
    );
    let parent_session_id = super::new_remote_session_id();
    let client = async_nats::connect(&url).await.expect("connect observer");
    let mut events = client
        .subscribe(crate::nats_event_sink::events_subject(&parent_session_id))
        .await
        .expect("subscribe parent progress");
    client.flush().await.expect("flush progress subscription");
    let toolset = progress_toolset(&url).await;
    let prompt = tokio::spawn(async move {
        toolset
            .invoke(
                "session_prompt",
                json!({
                    "message": "report progress",
                    "__harnx_parent_session_id": parent_session_id,
                }),
                CancellationToken::new(),
            )
            .await
    });

    let (running, terminal) =
        tokio::time::timeout(Duration::from_secs(10), observe_progress(&mut events))
            .await
            .expect("receive terminal sub-agent progress");
    assert!(!running.is_empty(), "heartbeat should report liveness");
    assert!(running
        .windows(2)
        .all(|pair| pair[0].elapsed_ms <= pair[1].elapsed_ms));
    assert_eq!(terminal.status, SubAgentProgressStatus::Done);

    let result = prompt.await.expect("join prompt").expect("prompt succeeds");
    let summary: SubAgentProgress = serde_json::from_value(result["sub_agent_progress"].clone())
        .expect("decode durable progress summary");
    assert_eq!(summary, terminal);

    daemon.abort();
    let _ = daemon.await;
    let _ = nats.kill();
    let _ = nats.wait();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn completed_results_include_progress_and_preserve_agent_marker() {
    let _env_guard = env_lock().await;
    let Some((url, mut nats, _store_dir)) = spawn_test_nats().await else {
        return;
    };
    let seeded = seed_remote_config(&url);
    let _env = subagent_test_env(&url, &seeded);
    let daemon = spawn_metis_worker_with_call_fn(&url, fixed_prompt_call_fn("marker response"));
    let toolset = test_subagent_toolset(&url).await;

    let created = toolset
        .invoke("session_new", json!({}), CancellationToken::new())
        .await
        .expect("create marked child session");
    let session_id = created["session_id"]
        .as_str()
        .expect("new result session id")
        .to_string();
    assert_completed_result(&created, &session_id);

    let prompted = toolset
        .invoke(
            "session_prompt",
            json!({ "message": "continue marked session", "session_id": session_id }),
            CancellationToken::new(),
        )
        .await
        .expect("prompt marked child session");
    assert_completed_result(&prompted, &session_id);
    assert_ne!(
        prompted["sub_agent_progress"]["invocation_id"],
        created["sub_agent_progress"]["invocation_id"],
        "reusing a session still creates a distinct progress invocation"
    );

    daemon.abort();
    let _ = daemon.await;
    let _ = nats.kill();
    let _ = nats.wait();
}

fn assert_completed_result(result: &serde_json::Value, session_id: &str) {
    assert_eq!(result["sub_agent"]["agent"], "metis");
    assert_eq!(result["sub_agent"]["session_id"], session_id);
    assert_eq!(result["session_id"], session_id);
    assert!(result["sub_agent"].get("model").is_none());
    assert_eq!(result["sub_agent_progress"]["status"], "done");
    assert_eq!(result["sub_agent_progress"]["agent"], "metis");
    assert_eq!(result["sub_agent_progress"]["session_id"], session_id);
    assert!(result["sub_agent_progress"]["invocation_id"]
        .as_str()
        .is_some_and(|id| !id.is_empty()));
}
