//! Real worker + BashKit confirm + counted NATS tool. Only the model is stubbed.
use std::{
    ffi::OsString,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use agent_client_protocol::v1::schema::{CancelNotification, StopReason};
use anyhow::{Context, Result};
use harnx_core::{instance::ServerScope, session::SessionLogEntry, tool::ToolCall};
use harnx_runtime::{config::GlobalConfig, nats_session_log::NatsSessionLog, AgentCallFn};
use serde_json::json;
use tokio_util::task::AbortOnDropHandle;

use super::super::support::*;

const ENV_KEYS: [&str; 3] = ["HARNX_SERVER_SCOPE", "HARNX_NATS_URL", "HARNX_NATS_TOKEN"];
struct TestEnv([Option<OsString>; 3]);
impl TestEnv {
    fn set(scope: &ServerScope, url: &str) -> Self {
        let previous = Self(ENV_KEYS.map(std::env::var_os));
        for (key, value) in ENV_KEYS.into_iter().zip([scope.as_str(), url, TOKEN]) {
            std::env::set_var(key, value);
        }
        previous
    }
}
impl Drop for TestEnv {
    fn drop(&mut self) {
        for (key, previous) in ENV_KEYS.into_iter().zip(&self.0) {
            match previous {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

struct Counter(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl harnx_toolset::Toolset for Counter {
    fn name(&self) -> &str {
        "counter"
    }
    fn tools(&self) -> Vec<harnx_toolset::ToolSpec> {
        vec![harnx_toolset::ToolSpec {
            name: "run".into(),
            description: "Count executions".into(),
            input_schema: json!({"type":"object"}),
            idempotent_hint: true,
            read_only_hint: false,
            timeout_secs: None,
            meta: None,
            cancellation_guarantee: Default::default(),
        }]
    }
    async fn invoke(
        &self,
        _: &str,
        _: serde_json::Value,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<serde_json::Value, harnx_toolset::ToolInvokeError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"counted":true}))
    }
}

fn model() -> AgentCallFn {
    let turns = AtomicUsize::new(0);
    Arc::new(move |_, _, _| {
        let first = turns.fetch_add(1, Ordering::SeqCst) == 0;
        Box::pin(async move {
            let calls = if first {
                vec![ToolCall::new(
                    "counter_run".into(),
                    json!({}),
                    Some("counted-call".into()),
                    None,
                )]
            } else {
                vec![]
            };
            Ok(("done".into(), None, calls, Default::default()))
        })
    })
}

async fn start_counter(
    config: &GlobalConfig,
    scope: ServerScope,
    url: String,
    calls: Arc<AtomicUsize>,
) -> Result<AbortOnDropHandle<Result<()>>> {
    let snapshot = config.read().clone();
    let js = snapshot.nats_jetstream(CLUSTER).await?;
    let key = harnx_toolset_server::registration_key(&scope, "____counter");
    let task = AbortOnDropHandle::new(tokio::spawn(async move {
        harnx_toolset_server::serve_over_nats(Counter(calls), scope, &url, TOKEN).await
    }));
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            if let Ok(bucket) = js
                .get_key_value(harnx_toolset_server::TOOL_REGISTRY_BUCKET)
                .await
            {
                if bucket.get(&key).await?.is_some() {
                    return Ok::<_, anyhow::Error>(());
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await??;
    Ok(task)
}

async fn round_trip(
    reply: PermissionReply,
    expected_calls: usize,
    cancel_turn: bool,
) -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let config = test_config(&server.url);
    {
        let mut cfg = config.write();
        cfg.data.use_tools = Some(vec!["counter_run".into()]);
        cfg.data.middleware = serde_json::from_value(json!([{
            "name":"approval", "match":"^counter_run$",
            "bash":"if confirm --id gate 'Run counted tool?'; then next; else deny 'Denied by user'; fi"
        }]))?;
    }
    let scope = ServerScope::new();
    let _env = TestEnv::set(&scope, &server.url);
    let calls = Arc::new(AtomicUsize::new(0));
    let _tool = start_counter(&config, scope, server.url.clone(), calls.clone()).await?;
    let _worker = AbortOnDropHandle::new(
        spawn_worker_with_config(
            config.clone(),
            model(),
            harnx_runtime::nats_worker::WorkerDaemonConfig::new(CLUSTER, "acp-confirm-worker"),
        )
        .await?,
    );
    let agent = new_agent(&config);
    let mut client = attach_test_client(agent.clone(), PermissionReply::Manual).await?;
    let session_id = initialize_and_create_session(&agent).await?;
    let task = {
        let agent = agent.clone();
        let session = session_id.clone();
        AbortOnDropHandle::new(tokio::spawn(async move {
            agent.prompt(text_prompt(session, "Run tool")).await
        }))
    };
    let request = tokio::time::timeout(TEST_TIMEOUT, client.permissions.recv())
        .await?
        .context("permission stream closed")?;
    assert_eq!(
        request.tool_call.fields.title.as_deref(),
        Some("Run counted tool?")
    );
    assert_eq!(request.tool_call.tool_call_id.0.as_ref(), "counted-call");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "tool can't run before answer"
    );
    if cancel_turn {
        agent
            .cancel(CancelNotification::new(session_id.clone()))
            .await?;
        client
            .permission_responses
            .send(super::harness::selected("allow"))?;
    } else {
        let outcome = match reply {
            PermissionReply::Allow => super::harness::selected("allow"),
            PermissionReply::Reject => super::harness::selected("reject"),
            PermissionReply::Cancelled => {
                agent_client_protocol::v1::schema::RequestPermissionOutcome::Cancelled
            }
            _ => unreachable!(),
        };
        client.permission_responses.send(outcome)?;
    }
    let response = tokio::time::timeout(TEST_TIMEOUT, task).await???;
    assert!(
        client.permissions.try_recv().is_err(),
        "middleware must not also open a legacy dialog"
    );
    assert_eq!(
        response.stop_reason,
        if cancel_turn {
            StopReason::Cancelled
        } else {
            StopReason::EndTurn
        }
    );
    assert_eq!(calls.load(Ordering::SeqCst), expected_calls);
    let snapshot = config.read().clone();
    let js = snapshot.nats_jetstream(CLUSTER).await?;
    let key = harnx_core::session_identity::session_key(None, session_id.0.as_ref());
    let log = NatsSessionLog::new_with_replicas(js, key, 1);
    let entries = log.load_events_latest_async().await?;
    let decisions: Vec<_> = entries
        .iter()
        .filter_map(|(_, entry)| match entry {
            SessionLogEntry::InteractionResponse {
                interaction_id,
                request_seq,
                response,
                ..
            } => Some((interaction_id, request_seq, response)),
            _ => None,
        })
        .collect();
    if cancel_turn {
        assert!(
            decisions.is_empty(),
            "late allow after session/cancel must not apply"
        );
    } else {
        assert_eq!(decisions.len(), 1);
        assert_eq!(
            decisions[0].2.action,
            if expected_calls == 1 {
                "approve"
            } else {
                "deny"
            }
        );
        assert!(decisions[0].2.note.is_none());
        let value = serde_json::to_value(request)?;
        assert_eq!(value["_meta"]["harnx:interaction_id"], *decisions[0].0);
        assert_eq!(value["_meta"]["harnx:request_seq"], *decisions[0].1);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn allow_once_resumes_middleware_and_runs_tool_once() -> Result<()> {
    round_trip(PermissionReply::Allow, 1, false).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reject_once_resumes_middleware_without_running_tool() -> Result<()> {
    round_trip(PermissionReply::Reject, 0, false).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_dialog_resumes_middleware_without_running_tool() -> Result<()> {
    round_trip(PermissionReply::Cancelled, 0, false).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_turn_drops_pending_middleware_permission_and_blocks_late_allow() -> Result<()> {
    round_trip(PermissionReply::Manual, 0, true).await
}
