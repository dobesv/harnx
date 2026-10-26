//! Real broker coverage for output-only bounds and identical stored/live replies.
mod failures;
mod fixture;

use anyhow::{Context, Result};
use fixture::*;
use futures_util::StreamExt;
use harnx_toolset::{ReplayAttempt, ToolErrorPayload, ToolReply};
use harnx_toolset_server::invocation_journal::RecordedInvocation;
use serde_json::json;
use std::{sync::atomic::Ordering, time::Duration};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_above_default_payload_preserves_original_and_sibling_across_restart() -> Result<()>
{
    let mut fixture = Fixture::start(None, None).await?;
    assert_eq!(fixture.client.server_info().max_payload, 1_048_576);
    let big = request("large-native", 2 * 1024 * 1024, true);
    let small = request("small-sibling", 64 * 1024, false);
    let (large, sibling) = tokio::try_join!(fixture.call(&big), fixture.call(&small))?;
    assert!(matches!(
        reply(&large).result,
        Err(ToolErrorPayload::Recoverable(_))
    ));
    assert_eq!(
        retained(&fixture, &reply(&large)).await?.result,
        Ok(output(&big.args))
    );
    assert_eq!(reply(&sibling).result, Ok(output(&small.args)));
    assert!(
        sibling.payload.len() > 32_768,
        "native replies have no bridge presentation cap"
    );
    fixture.check_stored(&big, &large, 1_048_576).await?;
    fixture.check_stored(&small, &sibling, 1_048_576).await?;
    // Exercise both the success cache and the journal path for size errors.
    assert_eq!(fixture.call(&small).await?.payload, sibling.payload);
    assert_eq!(fixture.call(&big).await?.payload, large.payload);
    fixture.restart().await?;
    let mut replay = big.clone();
    replay.replay = Some(ReplayAttempt {
        attempt: 1,
        requested_by: "replacement-worker".into(),
    });
    assert_eq!(fixture.call(&replay).await?.payload, large.payload);
    assert_eq!(fixture.call(&small).await?.payload, sibling.payload);
    assert_eq!(
        fixture.toolset.calls.load(Ordering::SeqCst),
        2,
        "no replay reexecution"
    );
    fixture.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reduced_wire_and_independent_kv_include_progress_and_nonterminal_fields() -> Result<()> {
    let mut fixture = Fixture::start(Some(16 * 1024), Some(8 * 1024)).await?;
    let mut request = request("escaped-output", 20_000, true);
    request.args["unit"] = json!("\"\\\n🦀");
    request.args["checkpoint"] = json!({"handle": "job-kept"});
    request.args["partial"] = json!({"partial": "kept"});
    request.args["progress_bytes"] = json!(10_000);
    request
        .capabilities
        .insert(harnx_toolset::CAPABILITY_TOOL_PROGRESS.into());
    let message = fixture.call(&request).await?;
    let record = fixture.check_stored(&request, &message, 8 * 1024).await?;
    assert_eq!(record.request, request);
    assert_eq!(record.checkpoint, Some(request.args["checkpoint"].clone()));
    assert_eq!(record.partial_result, Some(request.args["partial"].clone()));
    assert_eq!(
        reply(&message).final_progress,
        None,
        "large advisory snapshot retained only with original"
    );
    let original = retained(&fixture, &reply(&message)).await?;
    assert_eq!(original.result, Ok(output(&request.args)));
    assert_eq!(
        original.final_progress.unwrap().markdown.unwrap().len(),
        10_000
    );
    fixture.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn contextless_oversize_is_durable_explicit_error_not_fabricated_attachment() -> Result<()> {
    let mut fixture = Fixture::start(Some(16 * 1024), None).await?;
    let mut request = request("no-owner", 32 * 1024, false);
    request.args["parent_local_session_id"] = json!("forged");
    request.args["__harnx_parent_session_id"] = json!("forged");
    let message = fixture.call(&request).await?;
    let ToolErrorPayload::Recoverable(error) = reply(&message).result.unwrap_err() else {
        panic!("expected recoverable")
    };
    assert!(error.contains("not retained"));
    assert!(error.contains("side effects may already have occurred"));
    assert!(!error.contains("cid:"));
    fixture.check_stored(&request, &message, 16 * 1024).await?;
    assert_eq!(fixture.call(&request).await?.payload, message.payload);
    assert_eq!(fixture.toolset.calls.load(Ordering::SeqCst), 1);
    assert!(
        harnx_blob_store::media::optional_attachments_bucket(&fixture.js)
            .await?
            .is_none()
    );
    fixture.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exact_row_boundary_counts_cas_headers() -> Result<()> {
    const CAP: usize = 8192;
    let mut fixture = Fixture::start(None, Some(CAP as i32)).await?;
    for (index, delta) in [-1isize, 0, 1].into_iter().enumerate() {
        let request = request(&format!("boundary-{index}"), 1, false);
        fixture
            .journal
            .record(&request, ("echo", fixture.scope.as_str(), "____test"))
            .await?;
        let row = fixture.journal.get(&request).await?.context("row")?;
        let revision = fixture.store.entry(key(&request)).await?.unwrap().revision;
        let header_bytes = 51 + revision.to_string().len();
        let mut original = ToolReply {
            call_id: request.call_id.clone(),
            result: Ok(json!("")),
            final_progress: None,
        };
        let empty_reply_bytes = serde_json::to_vec(&original)?.len();
        let count = (CAP - header_bytes - serde_json::to_vec(&row)?.len() + 4 - empty_reply_bytes)
            .checked_add_signed(delta)
            .unwrap();
        original.result = Ok(json!("x".repeat(count)));
        let saved = fixture.journal.complete(&request, original.clone()).await?;
        if delta <= 0 {
            assert_eq!(saved, original);
        } else {
            assert!(matches!(
                saved.result,
                Err(ToolErrorPayload::Recoverable(_))
            ));
        }
        let message = fixture.call(&request).await?;
        fixture.check_stored(&request, &message, CAP).await?;
        if delta == 0 {
            let stored = fixture.store.entry(key(&request)).await?.unwrap();
            assert_eq!(stored.value.len() + header_bytes, CAP);
        }
    }
    assert_eq!(fixture.toolset.calls.load(Ordering::SeqCst), 0);
    fixture.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changed_kv_limit_recovers_real_10054_by_committing_terminal_without_reexecution(
) -> Result<()> {
    let mut fixture = Fixture::start(None, None).await?;
    let mut config = fixture.store.stream.cached_info().config.clone();
    config.max_message_size = 8192;
    fixture.js.update_stream(config).await?;
    // Server journal handle predates the changed limit. Observe the actual
    // rejected terminal publish and the bounded CAS retry, not helper estimates.
    let request = request("changed-limit", 32 * 1024, true);
    let mut writes = fixture
        .client
        .subscribe(format!(
            "$KV.{}.{}",
            harnx_toolset_server::invocation_journal::BUCKET,
            key(&request)
        ))
        .await?;
    crate::common::round_trip(&fixture.client).await?;
    let message = fixture.call(&request).await?;
    let mut terminal_sizes = Vec::new();
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let write = writes.next().await.context("journal writes closed")?;
            let row: RecordedInvocation = serde_json::from_slice(&write.payload)?;
            if let Some(reply) = row.reply {
                terminal_sizes.push(write.payload.len());
                if serde_json::to_vec(&reply)? == message.payload {
                    break;
                }
            }
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("observe terminal journal writes")??;
    assert_eq!(
        terminal_sizes.len(),
        2,
        "must observe rejected stale-limit write before retry: {terminal_sizes:?}"
    );
    assert!(terminal_sizes[0] > 8192);
    assert!(terminal_sizes[1] < 8192);
    fixture.check_stored(&request, &message, 8192).await?;
    assert_eq!(
        retained(&fixture, &reply(&message)).await?.result,
        Ok(output(&request.args))
    );
    assert_eq!(fixture.call(&request).await?.payload, message.payload);
    assert_eq!(fixture.toolset.calls.load(Ordering::SeqCst), 1);
    fixture.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn competing_completions_return_the_same_authoritative_bounded_winner() -> Result<()> {
    let mut fixture = Fixture::start(None, Some(8192)).await?;
    let request = request("competing", 1, true);
    fixture
        .journal
        .record(&request, ("echo", fixture.scope.as_str(), "____test"))
        .await?;
    let first = ToolReply {
        call_id: request.call_id.clone(),
        result: Ok(json!("a".repeat(32 * 1024))),
        final_progress: None,
    };
    let second = ToolReply {
        result: Ok(json!("b".repeat(48 * 1024))),
        ..first.clone()
    };
    let (winner, loser) = tokio::try_join!(
        fixture.journal.complete(&request, first.clone()),
        fixture.journal.complete(&request, second.clone())
    )?;
    assert_eq!(winner, loser);
    let original = retained(&fixture, &winner).await?;
    assert!(original == first || original == second);
    let message = fixture.call(&request).await?;
    assert_eq!(reply(&message), winner);
    fixture.check_stored(&request, &message, 8192).await?;
    fixture.restart().await?;
    assert_eq!(fixture.call(&request).await?.payload, message.payload);
    assert_eq!(fixture.toolset.calls.load(Ordering::SeqCst), 0);
    fixture.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attestation_is_committed_once_and_cache_restart_preserve_exact_bytes() -> Result<()> {
    use harnx_core::execution_context::{ExecutionContextObservation, EXECUTION_CONTEXT_NAMESPACE};
    let mut fixture = Fixture::start(None, None).await?;
    let directory = tempfile::tempdir()?;
    let observation = ExecutionContextObservation::observe(directory.path(), directory.path());
    let mut request = request("attested", 1, false);
    request
        .capabilities
        .insert(EXECUTION_CONTEXT_NAMESPACE.into());
    request.args["value"] = json!({"_meta": {"other": "kept", EXECUTION_CONTEXT_NAMESPACE: observation}, "content": []});
    let message = fixture.call(&request).await?;
    fixture.check_stored(&request, &message, 1_048_576).await?;
    assert!(
        reply(&message).result.unwrap()["_meta"][EXECUTION_CONTEXT_NAMESPACE]["provenance"]
            .is_object()
    );
    assert_eq!(fixture.call(&request).await?.payload, message.payload);
    fixture.restart().await?;
    assert_eq!(fixture.call(&request).await?.payload, message.payload);
    assert_eq!(fixture.toolset.calls.load(Ordering::SeqCst), 1);
    fixture.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_losing_candidate_never_replaces_or_rebounds_a_stored_winner() -> Result<()> {
    let mut fixture = Fixture::start(None, Some(8192)).await?;
    let mut request = request("losing-handler", 64 * 1024, true);
    request.args["park"] = json!(true);
    let message = {
        let pending = fixture.call(&request);
        tokio::pin!(pending);
        tokio::select! {
            _ = fixture.toolset.entered.notified() => {},
            result = &mut pending => anyhow::bail!("tool did not park: {result:?}"),
            _ = tokio::time::sleep(Duration::from_secs(60)) => anyhow::bail!("tool did not enter"),
        }
        let winner = ToolReply {
            call_id: request.call_id.clone(),
            result: Ok(json!({"winning": true})),
            final_progress: None,
        };
        fixture.journal.complete(&request, winner.clone()).await?;
        // If the losing candidate is offloaded before checking the row, this
        // sealed attachment store would incorrectly turn the winner into fatal.
        harnx_blob_store::create_or_open_attachments_bucket(&fixture.js, 1).await?;
        let stream = fixture.js.get_stream("OBJ_harnx_attachments").await?;
        let mut config = stream.cached_info().config.clone();
        config.sealed = true;
        fixture.js.update_stream(config).await?;
        fixture.toolset.release.notify_one();
        let message = pending.await?;
        assert_eq!(reply(&message), winner);
        message
    };
    fixture.check_stored(&request, &message, 8192).await?;
    fixture.restart().await?;
    assert_eq!(fixture.call(&request).await?.payload, message.payload);
    assert_eq!(fixture.toolset.calls.load(Ordering::SeqCst), 1);
    fixture.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_shaped_results_share_guard_and_retain_exact_metadata_and_error_status() -> Result<()> {
    let mut fixture = Fixture::start(None, Some(8192)).await?;
    let mut request = request("mcp-result", 32 * 1024, true);
    request.args["shape"] = json!("mcp");
    let message = fixture.call(&request).await?;
    assert!(matches!(
        reply(&message).result,
        Err(ToolErrorPayload::Recoverable(_))
    ));
    let original = retained(&fixture, &reply(&message)).await?;
    assert_eq!(original.result, Ok(output(&request.args)));
    assert_eq!(original.result.unwrap()["isError"], true);
    fixture.check_stored(&request, &message, 8192).await?;
    fixture.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_journal_fields_remain_readable_without_reexecution() -> Result<()> {
    let mut fixture = Fixture::start(None, None).await?;
    let request = request("legacy-row", 1, false);
    fixture
        .journal
        .record(&request, ("echo", fixture.scope.as_str(), "____test"))
        .await?;
    let row = fixture.journal.get(&request).await?.unwrap();
    let mut legacy = serde_json::to_value(row)?;
    legacy.as_object_mut().unwrap().remove("checkpoint");
    legacy.as_object_mut().unwrap().remove("partial_result");
    legacy["reply"] = json!({"call_id": request.call_id, "result": {"Ok": {"legacy": true}}});
    fixture
        .store
        .put(key(&request), serde_json::to_vec(&legacy)?.into())
        .await?;
    let message = fixture.call(&request).await?;
    assert_eq!(reply(&message).result, Ok(json!({"legacy": true})));
    fixture.restart().await?;
    assert_eq!(fixture.call(&request).await?.payload, message.payload);
    assert_eq!(fixture.toolset.calls.load(Ordering::SeqCst), 0);
    fixture.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execution_metadata_overhead_is_bounded_and_raw_original_is_retained() -> Result<()> {
    use harnx_core::execution_context::{ExecutionContextObservation, EXECUTION_CONTEXT_NAMESPACE};
    let mut fixture = Fixture::start(None, Some(8192)).await?;
    let directory = tempfile::tempdir()?;
    let observation = ExecutionContextObservation::observe(directory.path(), directory.path());
    let mut request = request("metadata-boundary", 1, true);
    request
        .capabilities
        .insert(EXECUTION_CONTEXT_NAMESPACE.into());
    fixture
        .journal
        .record(&request, ("echo", fixture.scope.as_str(), "____test"))
        .await?;
    let row = fixture.journal.get(&request).await?.unwrap();
    let mut original = ToolReply {
        call_id: request.call_id.clone(),
        result: Ok(json!({"output": "", "_meta": {EXECUTION_CONTEXT_NAMESPACE: observation}})),
        final_progress: None,
    };
    let remaining =
        8192 - 52 - (serde_json::to_vec(&row)?.len() - 4) - serde_json::to_vec(&original)?.len();
    original.result.as_mut().unwrap()["output"] = json!("x".repeat(remaining));
    *fixture.toolset.preset.lock().unwrap() = Some(original.result.clone().unwrap());
    let message = fixture.call(&request).await?;
    let saved = reply(&message);
    assert!(
        matches!(saved.result, Err(ToolErrorPayload::Recoverable(_))),
        "server attestation must count toward the cap"
    );
    assert_eq!(
        retained(&fixture, &saved).await?,
        original,
        "retain raw metadata before attestation"
    );
    fixture.check_stored(&request, &message, 8192).await?;
    fixture.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tight_reply_headroom_keeps_a_short_reference_before_omitting_it() -> Result<()> {
    let mut fixture = Fixture::start(None, Some(8192)).await?;
    let mut request = request("short-marker", 32 * 1024, true);
    request.args["padding"] = json!("");
    fixture
        .journal
        .record(&request, ("echo", fixture.scope.as_str(), "____test"))
        .await?;
    let mut row = fixture.journal.get(&request).await?.unwrap();
    let padding = 8192 - 52 - (serde_json::to_vec(&row)?.len() - 4) - 320;
    row.request.args["padding"] = json!("x".repeat(padding));
    request = row.request.clone();
    let before = fixture.store.entry(key(&request)).await?.unwrap();
    fixture
        .store
        .update(
            key(&request),
            serde_json::to_vec(&row)?.into(),
            before.revision,
        )
        .await?;
    let message = fixture.call(&request).await?;
    assert!(message.payload.len() <= 320);
    assert_eq!(
        retained(&fixture, &reply(&message)).await?.result,
        Ok(output(&request.args))
    );
    fixture.check_stored(&request, &message, 8192).await?;
    fixture.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raw_journal_metadata_contract_survives_while_legacy_wire_reply_is_sanitized() -> Result<()>
{
    use harnx_core::execution_context::{
        ExecutionContextObservation, ToolObservationProvenance, EXECUTION_CONTEXT_NAMESPACE,
    };
    let mut fixture = Fixture::start(None, None).await?;
    let directory = tempfile::tempdir()?;
    let mut observation = ExecutionContextObservation::observe(directory.path(), directory.path());
    observation.provenance = Some(ToolObservationProvenance::new(
        "forged", "forged", "forged", "forged",
    ));
    for opted_in in [false, true] {
        let mut request = request(&format!("raw-metadata-{opted_in}"), 1, false);
        if opted_in {
            request
                .capabilities
                .insert(EXECUTION_CONTEXT_NAMESPACE.into());
        }
        fixture
            .journal
            .record(&request, ("echo", fixture.scope.as_str(), "____test"))
            .await?;
        let original = ToolReply {
            call_id: request.call_id.clone(),
            result: Ok(
                json!({"answer": "legacy", "_meta": {EXECUTION_CONTEXT_NAMESPACE: observation}}),
            ),
            final_progress: None,
        };
        assert_eq!(
            fixture.journal.complete(&request, original.clone()).await?,
            original
        );
        assert_eq!(
            fixture.journal.completed_reply(&request).await?,
            Some(original.clone())
        );
        let message = fixture.call(&request).await?;
        let delivered = reply(&message).result.unwrap();
        if opted_in {
            let provenance = &delivered["_meta"][EXECUTION_CONTEXT_NAMESPACE]["provenance"];
            assert_eq!(provenance["server_scope"], fixture.scope.as_str());
            assert_eq!(provenance["server_identity"], "____test");
            assert_eq!(provenance["call_id"], request.call_id);
        } else {
            assert!(delivered.get("_meta").is_none());
        }
        assert_eq!(
            fixture.journal.get(&request).await?.unwrap().reply,
            Some(original),
            "legacy raw row is still available to worker recovery"
        );
    }
    assert_eq!(fixture.toolset.calls.load(Ordering::SeqCst), 0);
    fixture.stop().await
}
