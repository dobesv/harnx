//! Barriers hold the interrupted turn's model drop while the same worker runs
//! the next one.
use super::*;
use tokio_util::task::AbortOnDropHandle;

#[derive(Default)]
struct DropBarrier {
    entered: Notify,
    released: std::sync::Mutex<bool>,
    wake: std::sync::Condvar,
}

impl DropBarrier {
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.wake.notify_all();
    }
}

struct ReleaseOnDrop(Arc<DropBarrier>);
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}

struct BlockedModelDrop(Arc<DropBarrier>);
impl Drop for BlockedModelDrop {
    fn drop(&mut self) {
        tokio::task::block_in_place(|| {
            self.0.entered.notify_one();
            let released = self.0.released.lock().unwrap();
            drop(
                self.0
                    .wake
                    .wait_while(released, |released| !*released)
                    .unwrap(),
            );
        });
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_worker_runs_g2_while_g1_model_drop_is_blocked() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let barrier = Arc::new(DropBarrier::default());
    let _release = ReleaseOnDrop(barrier.clone());
    let entered = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let call_fn: harnx_runtime::AgentCallFn = Arc::new({
        let barrier = barrier.clone();
        let entered = entered.clone();
        let calls = calls.clone();
        move |input, _, _| {
            let first = calls.fetch_add(1, Ordering::SeqCst) == 0;
            let barrier = barrier.clone();
            let entered = entered.clone();
            let prompt = input.text();
            Box::pin(async move {
                if first {
                    let _drop = BlockedModelDrop(barrier);
                    entered.notify_one();
                    std::future::pending::<()>().await;
                }
                assert_eq!(prompt, "G2");
                Ok(("G2 response".into(), None, vec![], Default::default()))
            })
        }
    });
    let daemon = AbortOnDropHandle::new(
        spawn_worker_daemon_with_call_fn(
            local_nats_runtime_config(server.url()),
            "overlap-worker",
            call_fn,
        )
        .await?,
    );
    let first = session(server.url(), "blocked-drop-overlap").await?;
    first.enqueue_text("G1").await?;
    tokio::time::timeout(CI_SAFE_TIMEOUT, entered.notified()).await?;
    assert!(first.cancel_pending_turn().await?);
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    await_cancel_entry(&js, first.storage_key()).await?;
    tokio::time::timeout(CI_SAFE_TIMEOUT, barrier.entered.notified()).await?;

    let second = session(server.url(), first.session_id()).await?;
    let result = tokio::time::timeout(
        CI_SAFE_TIMEOUT,
        second.run_turn("G2", Arc::new(NullSink), None),
    )
    .await??;
    assert_eq!(result.response.as_deref(), Some("G2 response"));
    assert!(!result.was_cancelled);
    assert!(!*barrier.released.lock().unwrap());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    barrier.release();
    daemon.abort();
    Ok(())
}

/// A follower returns the moment the log carries a `Cancel` for its turn.
/// No worker, activation or wind-up has to run first, and the interrupt adds
/// exactly one entry to the transcript.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn foreign_follower_returns_on_root_receipt_without_transcript_or_cleanup() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let session = session(server.url(), "stop-without-projection").await?;
    let config = local_nats_runtime_config(server.url());
    let input = harnx_runtime::config::input::from_str(&config, "G1", None);
    let admitted = session.admit_input(&input, None).await?;
    let client = async_nats::connect(server.url()).await?;
    let js = async_nats::jetstream::new(client);
    let log =
        harnx_runtime::nats_session_log::NatsSessionLog::new(js.clone(), session.storage_key());
    let before = log.load_events_latest_async().await?;

    // No worker is running: only the durable Cancel ends the follow.
    let cancel_seq = append_cancel(&js, session.storage_key()).await?;

    let result = tokio::time::timeout(
        CI_SAFE_TIMEOUT,
        session.follow_admitted_prompt(
            admitted,
            Arc::new(NullSink),
            None,
            None,
            Default::default(),
        ),
    )
    .await??;
    assert!(result.was_cancelled);
    assert!(result.response.is_none() && result.error.is_none());
    let after = log.load_events_latest_async().await?;
    assert_eq!(after.len(), before.len() + 1, "only the Cancel was written");
    assert!(matches!(&after[after.len() - 1], (seq, Entry::Cancel { .. }) if *seq == cancel_seq));
    Ok(())
}
