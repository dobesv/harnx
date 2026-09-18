use super::tests::spawn_test_nats;
use anyhow::{Context, Result};

/// Concurrent/replayed reservation attempts for the same invocation converge
/// on the same candidate session ID even when no checkpoint was ever written
/// for it (the owning worker crashed before it could record one).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_allocation_survives_a_crash_before_checkpointing() -> Result<()> {
    let (url, mut nats, _store) = spawn_test_nats().await.context("nats-server required")?;
    let js = async_nats::jetstream::new(async_nats::connect(&url).await?);
    let store = crate::nats_session_metadata::SessionMetadataStore::ensure(&js, 1).await?;
    let initializer = crate::SessionInitializer::named("metis", Default::default());
    let reserve = |id| {
        crate::utils::session_name::reserve_invocation_session_id(
            &store,
            &initializer,
            id,
            1_780_000_000_000,
        )
    };
    let first = reserve("parent/first").await?;
    // No checkpoint is written. Another allocation occupies the next candidate.
    let second = reserve("parent/second").await?;
    let (replayed, concurrent) = tokio::join!(reserve("parent/first"), reserve("parent/first"));
    assert_eq!(first.len(), 6);
    assert_ne!(first, second);
    assert_eq!(replayed?, first);
    assert_eq!(concurrent?, first);
    let _ = nats.kill();
    let _ = nats.wait();
    Ok(())
}
