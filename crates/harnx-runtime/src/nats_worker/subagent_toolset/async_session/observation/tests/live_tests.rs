//! Real worker keeps running after soft expiry and observer-only cancellation.
use super::{fixture::*, *};
use crate::nats_worker::tests::{
    env_lock, seed_remote_config, spawn_metis_worker_with_call_fn, subagent_test_env,
};
use harnx_core::session::SessionLogEntry;
use std::{sync::Arc, time::Duration};
use tokio::sync::Notify;

fn gated_model(entered: Arc<Notify>, release: Arc<Notify>) -> crate::agent_loop::AgentCallFn {
    Arc::new(move |_, _, _| {
        let entered = entered.clone();
        let release = release.clone();
        Box::pin(async move {
            entered.notify_one();
            release.notified().await;
            Ok((
                "child continued after observation".into(),
                None,
                vec![],
                Default::default(),
            ))
        })
    })
}

async fn initial_observation(
    f: &Fixture,
    entered: &Notify,
    cancel_observer: bool,
) -> Result<Value> {
    let cancel = CancellationToken::new();
    let mut args = f.request.args.clone();
    args["wait"] = json!({"seconds": if cancel_observer { 120 } else { 1 }});
    let observation = send(&f.toolset, args, cancel.clone(), f.send_context());
    tokio::pin!(observation);
    if cancel_observer {
        tokio::select! {
            result = &mut observation => anyhow::bail!("observer returned before cancellation: {result:?}"),
            result = tokio::time::timeout(Duration::from_secs(30), entered.notified()) => {
                result.context("worker never entered model")?;
                cancel.cancel();
            },
        }
    }
    let result = observation.await?;
    if !cancel_observer {
        tokio::time::timeout(Duration::from_secs(30), entered.notified())
            .await
            .context("worker never entered model")?;
    }
    Ok(result)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_worker_completes_after_initial_soft_expiry_and_after_observer_cancellation(
) -> Result<()> {
    let _env_guard = env_lock().await;
    for cancel_observer in [false, true] {
        let f = Fixture::configured("metis", "local", "live-child").await?;
        let seeded = seed_remote_config(f.server.url());
        let _env = subagent_test_env(f.server.url(), &seeded);
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let daemon = spawn_metis_worker_with_call_fn(
            f.server.url(),
            gated_model(entered.clone(), release.clone()),
        );
        let first = initial_observation(&f, &entered, cancel_observer).await?;
        assert_eq!(first["status"], "pending");
        assert_eq!(first["wait_expired"], !cancel_observer);
        let before = f
            .toolset
            .session_metadata
            .get_invocation_limits(f.log.storage_key(), "original-prompt")
            .await?
            .context("worker limits")?;
        release.notify_one();
        let completed = f.poll(json!({"seconds": 10})).await?;
        daemon.abort();
        let _ = daemon.await;
        assert_eq!(completed["status"], "completed");
        assert!(completed["output"]
            .to_string()
            .contains("child continued after observation"));
        let after = f
            .toolset
            .session_metadata
            .get_invocation_limits(f.log.storage_key(), "original-prompt")
            .await?
            .context("worker limits")?;
        assert_eq!(
            before, after,
            "observation must not change execution limits"
        );
        assert!(!f
            .log
            .load_events_latest_async()
            .await?
            .iter()
            .any(|(_, entry)| matches!(entry, SessionLogEntry::Cancel { .. })));
    }
    Ok(())
}
