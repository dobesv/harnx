//! Malformed tool syntax is recoverable; storage corruption is covered separately.
use super::*;
use harnx_toolset_server::invocation_journal::InvocationJournal;
use tokio_util::task::AbortOnDropHandle;

#[path = "malformed/tools.rs"]
mod tools;
#[path = "malformed/transcript.rs"]
mod transcript;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_image_keeps_siblings_and_next_inference_across_journal_replay() -> Result<()> {
    require_nextest();
    let mut fixture = ImageFixture::start_with_calls(&[
        ("malformed", "bad-call"),
        ("large", "image-call"),
        ("sibling", "sibling-call"),
    ])
    .await?;
    let calls = Arc::new(AtomicUsize::new(0));
    let _native = tools::start(&fixture.env, calls.clone());
    fixture
        .bridge
        .wait_for_servers(&fixture.env, &["images", "malformed"])
        .await?;
    fixture.session.run_turn(None).await?;
    fixture.image.assert_cached()?;
    let log = NatsSessionLog::new_with_replicas(fixture.env.js.clone(), &fixture.session.key, 1);
    let live = transcript::results(&log).await?;
    transcript::assert_results(&live);
    let journal = InvocationJournal::ensure(&fixture.env.js, 1).await?;
    let rows = journal.records_for_session(&fixture.session.key).await?;
    assert_eq!(rows.len(), 3);
    let bad = rows
        .iter()
        .find(|row| row.request.tool_call_id.as_deref() == Some("bad-call"))
        .unwrap();
    assert_eq!(
        bad.reply.as_ref().unwrap().result.as_ref().unwrap()["content"],
        tools::malformed_output()["content"]
    );

    // Leave the original ToolCalls and raw journal completions, but no matching
    // ToolResults. A fresh activation must repair the orphan from saved replies.
    transcript::retract_completion(&log).await?;
    std::fs::remove_dir_all(&fixture.image.cache)?;
    fixture.image.assert_cache_missing();
    fixture.session.replay().await?;
    fixture.image.assert_cached()?;
    let replayed = transcript::results(&log).await?;
    transcript::assert_results(&replayed);
    assert_eq!(
        replayed.iter().map(|r| &r.output).collect::<Vec<_>>(),
        live.iter().map(|r| &r.output).collect::<Vec<_>>(),
        "journal replay must preserve every tool output"
    );
    fixture.image.corrupt_remote(&fixture.env.js).await?;
    fixture.session.replay().await?;
    let requests = fixture.provider.finish().await?;
    fixture.image.assert_provider_images(&requests);
    for request in requests.iter().skip(1) {
        transcript::assert_provider_results(request);
    }
    assert_eq!(fixture.bridge.call_count()?, 1);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "replay must not repeat native side effects"
    );
    fixture.bridge.stop().await
}
