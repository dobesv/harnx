use super::fixture::*;
use anyhow::{Context, Result};
use harnx_toolset::{CheckpointStore, ToolErrorPayload};
use serde_json::json;
use std::{sync::atomic::Ordering, time::Duration};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attachment_store_failure_stays_fatal_and_does_not_claim_completion() -> Result<()> {
    let mut fixture = Fixture::start(None, Some(8192)).await?;
    harnx_blob_store::create_or_open_attachments_bucket(&fixture.js, 1).await?;
    let stream = fixture.js.get_stream("OBJ_harnx_attachments").await?;
    let mut config = stream.cached_info().config.clone();
    config.sealed = true;
    fixture.js.update_stream(config).await?;
    let request = request("sealed-attachment", 32 * 1024, true);
    let message = fixture.call(&request).await?;
    assert!(
        matches!(reply(&message).result, Err(ToolErrorPayload::Fatal(message)) if message.contains("retain oversized tool reply"))
    );
    assert!(fixture
        .journal
        .get(&request)
        .await?
        .unwrap()
        .reply
        .is_none());
    assert_eq!(fixture.toolset.calls.load(Ordering::SeqCst), 1);
    fixture.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_journal_outage_after_execution_remains_fatal() -> Result<()> {
    let fixture = Fixture::start(None, None).await?;
    let mut request = request("journal-outage", 2 * 1024 * 1024, true);
    request.args["park"] = json!(true);
    let pending = fixture.call(&request);
    tokio::pin!(pending);
    tokio::select! {
        _ = fixture.toolset.entered.notified() => {},
        result = &mut pending => anyhow::bail!("tool replied before barrier: {result:?}"),
        _ = tokio::time::sleep(Duration::from_secs(60)) => anyhow::bail!("tool never entered"),
    }
    fixture.js.delete_stream(&fixture.store.stream_name).await?;
    fixture.toolset.release.notify_one();
    let message = pending.await?;
    assert!(
        matches!(reply(&message).result, Err(ToolErrorPayload::Fatal(message)) if message.contains("tool invocation recovery"))
    );
    assert_eq!(fixture.toolset.calls.load(Ordering::SeqCst), 1);
    // Release the immutable borrow held by the pinned call before stop().
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn corrupt_invocation_is_not_downgraded_or_reexecuted() -> Result<()> {
    let mut fixture = Fixture::start(None, None).await?;
    let request = request("corrupt-row", 2 * 1024 * 1024, true);
    fixture
        .store
        .put(key(&request), "not a journal record".into())
        .await?;
    let message = fixture.call(&request).await?;
    assert!(matches!(
        reply(&message).result,
        Err(ToolErrorPayload::Fatal(_))
    ));
    assert_eq!(fixture.toolset.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        fixture.store.get(key(&request)).await?.unwrap().as_ref(),
        b"not a journal record"
    );
    fixture.stop().await
}

async fn saturated_row(input: bool) -> Result<()> {
    const CAP: usize = 16 * 1024;
    let mut fixture = Fixture::start(None, Some(CAP as i32)).await?;
    let mut request = request("saturated-row", 32 * 1024, false);
    fixture
        .journal
        .record(&request, ("echo", fixture.scope.as_str(), "____test"))
        .await?;
    let mut row = fixture.journal.get(&request).await?.context("row")?;
    let entry = fixture.store.entry(key(&request)).await?.unwrap();
    let headers = 51 + entry.revision.to_string().len();
    if input {
        row.request.args["padding"] = json!("");
        let remaining = CAP - headers - serde_json::to_vec(&row)?.len();
        row.request.args["padding"] = json!("x".repeat(remaining));
        request = row.request.clone();
        fixture
            .store
            .update(
                key(&request),
                serde_json::to_vec(&row)?.into(),
                entry.revision,
            )
            .await?;
    } else {
        row.checkpoint = Some(json!(""));
        let remaining = CAP - headers - serde_json::to_vec(&row)?.len();
        let checkpoint = json!("x".repeat(remaining));
        fixture
            .journal
            .checkpoint_store(&request)
            .checkpoint(checkpoint)
            .await?;
    }
    let before = fixture.store.entry(key(&request)).await?.unwrap();
    assert_eq!(
        before.value.len() + headers,
        CAP,
        "accepted nonterminal row fills the cap"
    );
    let message = fixture.call(&request).await?;
    assert!(
        matches!(reply(&message).result, Err(ToolErrorPayload::Fatal(message)) if message.contains("bounded terminal tool reply cannot fit"))
    );
    let after = fixture.store.entry(key(&request)).await?.unwrap();
    assert_eq!(after.value, before.value);
    assert_eq!(after.revision, before.revision);
    assert!(fixture
        .journal
        .get(&request)
        .await?
        .unwrap()
        .reply
        .is_none());
    assert_eq!(fixture.toolset.calls.load(Ordering::SeqCst), 1);
    fixture.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saturated_input_retains_explicit_fatal_durability_exception() -> Result<()> {
    saturated_row(true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saturated_checkpoint_retains_explicit_fatal_durability_exception() -> Result<()> {
    saturated_row(false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_fatal_and_interrupted_categories_are_never_downgraded() -> Result<()> {
    let mut fixture = Fixture::start(None, Some(8192)).await?;
    for (error, reason_count) in [
        ("fatal", 1),
        ("fatal", 32 * 1024),
        ("interrupted", 1),
        ("interrupted", 32 * 1024),
    ] {
        let mut request = request(&format!("{error}-{reason_count}"), 1, true);
        request.args["error"] = json!(error);
        request.args["reason_count"] = json!(reason_count);
        let message = fixture.call(&request).await?;
        let reply = reply(&message);
        assert_terminal_category(reply.result.as_ref().unwrap_err(), error);
        fixture.check_stored(&request, &message, 8192).await?;
        if reason_count > 1 {
            let original = retained(&fixture, &reply).await?;
            let reason = assert_terminal_category(original.result.as_ref().unwrap_err(), error);
            assert_eq!(reason, "x".repeat(reason_count));
        }
    }
    fixture.stop().await
}

fn assert_terminal_category<'a>(error: &'a ToolErrorPayload, category: &str) -> &'a str {
    match error {
        ToolErrorPayload::Fatal(reason) => {
            assert_eq!(category, "fatal");
            reason
        }
        ToolErrorPayload::Interrupted(interrupted) => {
            assert_eq!(category, "interrupted");
            assert_eq!(
                interrupted.cancellation_id.as_deref(),
                Some("cancel-original")
            );
            &interrupted.reason
        }
        other => panic!("terminal category changed: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sealed_journal_write_is_not_a_recoverable_size_failure() -> Result<()> {
    let mut fixture = Fixture::start(None, None).await?;
    let mut request = request("sealed-journal", 64 * 1024, false);
    request.args["park"] = json!(true);
    let message = {
        let pending = fixture.call(&request);
        tokio::pin!(pending);
        tokio::select! {
            _ = fixture.toolset.entered.notified() => {},
            result = &mut pending => anyhow::bail!("tool did not park: {result:?}"),
            _ = tokio::time::sleep(Duration::from_secs(60)) => anyhow::bail!("tool did not enter"),
        }
        let mut config = fixture.store.stream.cached_info().config.clone();
        config.sealed = true;
        fixture.js.update_stream(config).await?;
        fixture.toolset.release.notify_one();
        pending.await?
    };
    assert!(
        matches!(reply(&message).result, Err(ToolErrorPayload::Fatal(message)) if message.contains("persist terminal tool invocation reply"))
    );
    assert!(fixture
        .journal
        .get(&request)
        .await?
        .unwrap()
        .reply
        .is_none());
    assert_eq!(fixture.toolset.calls.load(Ordering::SeqCst), 1);
    fixture.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn journal_publish_permission_denial_is_fatal_not_size_recovery() -> Result<()> {
    let config = r#"authorization { users: [{user: "limited", password: "test", permissions: {publish: {allow: [">"], deny: ["$KV.harnx_tool_invocations.>"]}, subscribe: [">"]}}] }"#;
    let mut fixture = Fixture::configured(Some(config), None).await?;
    let request = request("permission-denied", 64 * 1024, false);
    fixture
        .journal
        .record(&request, ("echo", fixture.scope.as_str(), "____test"))
        .await?;
    let (events_tx, mut events) = tokio::sync::mpsc::unbounded_channel();
    let client = async_nats::ConnectOptions::new()
        .user_and_password("limited".into(), "test".into())
        .event_callback(move |event| {
            let events_tx = events_tx.clone();
            async move {
                let _ = events_tx.send(event.to_string());
            }
        })
        .connect(&fixture.broker.url)
        .await?;
    fixture.serve_client(client).await?;
    let message = fixture.call(&request).await?;
    assert!(matches!(
        reply(&message).result,
        Err(ToolErrorPayload::Fatal(_))
    ));
    tokio::time::timeout(Duration::from_secs(60), async {
        while let Some(event) = events.recv().await {
            if event.to_lowercase().contains("permissions violation") {
                return Ok(());
            }
        }
        anyhow::bail!("no actual permission violation received")
    })
    .await??;
    assert!(fixture
        .journal
        .get(&request)
        .await?
        .unwrap()
        .reply
        .is_none());
    assert_eq!(fixture.toolset.calls.load(Ordering::SeqCst), 1);
    fixture.stop().await
}
