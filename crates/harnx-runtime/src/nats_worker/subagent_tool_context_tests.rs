use super::subagent_toolset::SubagentToolset;
use super::tests::{
    echoing_call_fn, env_lock, seed_remote_config, spawn_metis_worker_with_call_fn,
    spawn_test_nats, subagent_test_env, test_subagent_toolset,
};
use crate::nats_session_log::NatsSessionLog;
use crate::nats_session_metadata::{SessionMetadataStore, ToolContextEntry};
use crate::{NatsSession, NatsSessionConfig, SessionActivationRoute, SessionInitializer};
use harnx_toolset::{ToolInvocation, ToolInvocationContext, Toolset};
use serde_json::json;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

struct ParentBinding {
    session_id: String,
    metadata: SessionMetadataStore,
    _session: NatsSession,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn new_inherits_context_then_prompt_reuse_and_load_share_one_session_log() {
    let _env_guard = env_lock().await;
    let Some((url, mut nats, _store_dir)) = spawn_test_nats().await else {
        return;
    };
    let seeded = seed_remote_config(&url);
    let _env = subagent_test_env(&url, &seeded);
    let captured = Arc::new(Mutex::new(Vec::new()));
    let daemon = spawn_metis_worker_with_call_fn(&url, echoing_call_fn(Arc::clone(&captured)));
    let toolset = test_subagent_toolset(&url).await;
    let parent = create_bound_parent(&url).await;

    let session_id = create_inheriting_child(&toolset, &parent.session_id).await;
    assert_inherited_context(&parent.metadata, &session_id).await;
    let log = child_log(&seeded.parent_config, &session_id).await;
    let after_new = log.load_events_async().await.unwrap().len();
    exercise_reused_child(&toolset, &session_id, after_new).await;
    assert!(log.load_events_async().await.unwrap().len() > after_new);
    assert_eq!(
        captured.lock().await.as_slice(),
        [
            "Start a new session.",
            "first continuation",
            "second continuation"
        ]
    );

    daemon.abort();
    let _ = daemon.await;
    let _ = nats.kill();
    let _ = nats.wait();
}

async fn create_bound_parent(url: &str) -> ParentBinding {
    let client = async_nats::connect(url).await.unwrap();
    let jetstream = async_nats::jetstream::new(client.clone());
    let session_id = super::new_remote_session_id();
    let session = NatsSession::new(
        NatsSessionConfig {
            cluster: "local".to_string(),
            initializer: SessionInitializer::named("metis", Default::default()),
            session_id: Some(session_id.clone()),
            activation_route: SessionActivationRoute::ClusterShared,
        },
        client,
        jetstream.clone(),
        harnx_core::abort::create_abort_signal(),
    )
    .await
    .unwrap();
    let metadata = SessionMetadataStore::ensure(&jetstream, 1).await.unwrap();
    metadata
        .replace_tool_context_value(
            ToolContextEntry {
                session_id: &session_id,
                key: "sandbox",
            },
            json!({"version": 1, "sandbox_id": "sandbox-inherited"}),
        )
        .await
        .unwrap();
    ParentBinding {
        session_id,
        metadata,
        _session: session,
    }
}

async fn create_inheriting_child(toolset: &SubagentToolset, parent_session_id: &str) -> String {
    let created = toolset
        .invoke_with_context(ToolInvocation {
            tool: "session_new".to_string(),
            args: json!({}),
            context: ToolInvocationContext {
                operation: None,
                call_id: "parent-delegation".to_string(),
                invoking_session_id: Some(parent_session_id.to_string()),
                capabilities: Default::default(),
            },
            cancel: CancellationToken::new(),
        })
        .await
        .unwrap();
    created["session_id"].as_str().unwrap().to_string()
}

async fn assert_inherited_context(metadata: &SessionMetadataStore, session_id: &str) {
    let context = metadata
        .get_tool_context(session_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        context.values.get("sandbox"),
        Some(&json!({"version": 1, "sandbox_id": "sandbox-inherited"}))
    );
}

async fn child_log(config: &crate::config::Config, session_id: &str) -> NatsSessionLog {
    NatsSessionLog::new(
        config.nats_jetstream("local").await.unwrap(),
        session_id.to_string(),
    )
}

async fn exercise_reused_child(toolset: &SubagentToolset, session_id: &str, after_new: usize) {
    for message in ["first continuation", "second continuation"] {
        let result = toolset
            .invoke(
                "session_prompt",
                json!({"message": message, "session_id": session_id}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(result["session_id"], session_id);
        assert_eq!(
            result["response"],
            format!("stub remote reply over nats: {message}")
        );
    }
    let loaded = toolset
        .invoke(
            "session_load",
            json!({"session_id": session_id}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(loaded["events"].as_array().unwrap().len() > after_new);
}
