use super::*;
use futures::StreamExt;
use harnx_runtime::fault_injection::t2_1_preflight_count;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio_util::task::AbortOnDropHandle;

#[derive(Clone, Copy)]
pub(super) enum MixedFixtureKind {
    Fixture2,
    Fixture4a,
    Fixture4b,
}

struct LegacyWorker(String);

impl LegacyWorker {
    fn start(storage: &str) -> Self {
        simulate_t2_1_worker(storage, true);
        Self(storage.into())
    }
}

impl Drop for LegacyWorker {
    fn drop(&mut self) {
        simulate_t2_1_worker(&self.0, false);
    }
}

async fn setup_tool_probe(t: &TwoBackends) -> Result<(Arc<AtomicU64>, AbortOnDropHandle<()>)> {
    let counter = Arc::new(AtomicU64::new(0));
    let mut subscriber =
        t.h.jetstream
            .client()
            .subscribe("harnx.v1.*.tools.*.get_current_time")
            .await?;
    t.h.jetstream.query_account().await?;
    let observed = counter.clone();
    let task = tokio::spawn(async move {
        while subscriber.next().await.is_some() {
            observed.fetch_add(1, Ordering::SeqCst);
        }
    });
    Ok((counter, AbortOnDropHandle::new(task)))
}

async fn park_old_worker(
    t: &TwoBackends,
    setup: &mut InteractionOwnedSetup,
    foreground: bool,
) -> Result<()> {
    let legacy = LegacyWorker::start(setup.session.storage_key());
    run_recovery(t, &setup.session, &setup.task_id, foreground).await?;
    let old_count = t2_1_preflight_count(setup.session.storage_key());
    assert!(setup.session.activate_pending_turn().await?.is_some());
    tokio::time::timeout(DEADLINE, async {
        while t2_1_preflight_count(setup.session.storage_key()) <= old_count {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await?;
    // Fixture 4b's target is canonically admitted and may already have completed.
    // The old-worker assertion's one-message target is a different, raw fixture.
    let target = setup.target_storage.take();
    assert_interaction_owned_preserved(t, setup).await?;
    setup.target_storage = target;
    let tick = ticket(&setup.before, setup.session.storage_key())?;
    assert!(matches!(
        setup.session.fixed_prompt_progress(&tick).await?,
        harnx_runtime::PromptProgress::Parked
    ));
    drop(legacy);
    Ok(())
}

async fn run_mixed_version_test(kind: MixedFixtureKind, foreground: bool) -> Result<()> {
    let t = TwoBackends::start_script(Script::ReadOnlyTool).await?;
    let (counter, _probe) = setup_tool_probe(&t).await?;
    let mut setup = match kind {
        MixedFixtureKind::Fixture2 => fixtures::mixed_fixture_2_setup(&t).await?,
        MixedFixtureKind::Fixture4a => fixtures::mixed_fixture_4a_setup(&t).await?,
        MixedFixtureKind::Fixture4b => fixtures::mixed_fixture_4b_setup(&t).await?,
    };
    park_old_worker(&t, &mut setup, foreground)
        .await
        .with_context(|| format!("old worker parking: {}", t.h.logs.text()))?;
    assert_eq!(
        counter.load(Ordering::SeqCst),
        0,
        "old worker must not execute tools"
    );
    assert!(setup.session.activate_pending_turn().await?.is_some());
    let tick = ticket(&setup.before, setup.session.storage_key())?;
    tokio::time::timeout(DEADLINE, async {
        loop {
            if let Some(result) = setup.session.fixed_prompt_completion(&tick).await? {
                assert!(
                    result.error.is_none(),
                    "resumed invocation failed: {:?}",
                    result.error
                );
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .with_context(|| format!("runtime completion: {}", t.h.logs.text()))??;
    let response =
        t.b.rpc("alice", "GetTask", json!({"id": setup.task_id}))
            .await?;
    assert_eq!(
        response["result"]["status"]["state"],
        "TASK_STATE_COMPLETED"
    );
    t.h.jetstream.query_account().await?;
    assertions::assert_mixed_version_completed(&t, &setup, kind, counter.load(Ordering::SeqCst))
        .await?;
    teardown(&t, setup);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_interaction_owned_fixture_2_foreground_resumes_t2_2_worker_completes(
) -> Result<()> {
    run_mixed_version_test(MixedFixtureKind::Fixture2, true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_interaction_owned_fixture_2_background_sweep_resumes_t2_2_worker_completes(
) -> Result<()> {
    run_mixed_version_test(MixedFixtureKind::Fixture2, false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_interaction_owned_fixture_4a_foreground_resumes_t2_2_worker_completes(
) -> Result<()> {
    run_mixed_version_test(MixedFixtureKind::Fixture4a, true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_interaction_owned_fixture_4a_background_sweep_resumes_t2_2_worker_completes(
) -> Result<()> {
    run_mixed_version_test(MixedFixtureKind::Fixture4a, false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_interaction_owned_fixture_4b_foreground_resumes_t2_2_worker_completes(
) -> Result<()> {
    run_mixed_version_test(MixedFixtureKind::Fixture4b, true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_interaction_owned_fixture_4b_background_sweep_resumes_t2_2_worker_completes(
) -> Result<()> {
    run_mixed_version_test(MixedFixtureKind::Fixture4b, false).await
}
