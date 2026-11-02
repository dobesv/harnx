//! Exercise storage and fallback branches through the real backend append.
use super::*;
use anyhow::Context;
use base64::Engine;
use harnx_core::{
    attachments::cid_for_data_url_with_session, cid_url::SessionRef, session::ToolOutput,
};
use harnx_toolset_server::reply_limits::{
    full_size_error_message, minimal_size_error_message, short_size_error_message,
};

const BUDGET: usize = 64 * 1024 - 1024;

fn output(id: &str, bytes: usize) -> ToolOutput {
    ToolOutput {
        id: Some(id.into()),
        name: "fallback_tool".into(),
        output: json!({"out": "x".repeat(bytes)}),
        markdown: None,
        content: Vec::new(),
        switch_agent: None,
    }
}

fn entry(results: Vec<ToolOutput>) -> SessionLogEntry {
    SessionLogEntry::ToolResults {
        results,
        timestamp: None,
    }
}

fn size(entry: &SessionLogEntry) -> usize {
    harnx_runtime::nats_session_log::serialize_entry(entry)
        .unwrap()
        .len()
}

fn with_error(original: &ToolOutput, message: String) -> ToolOutput {
    let mut result = original.clone();
    result.output = json!({"is_error": true, "error": message});
    result
}

fn retained_url(owner: &SessionRef, original: &ToolOutput) -> String {
    let data = format!(
        "data:application/json;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(original).unwrap())
    );
    cid_for_data_url_with_session(owner, &data)
}

async fn appended(log: &NatsSessionLog, seq: u64) -> Result<SessionLogEntry> {
    log.load_events_async()
        .await?
        .into_iter()
        .find_map(|(n, entry)| (n == seq).then_some(entry))
        .context("appended entry")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retention_failure_still_appends_an_honest_size_error() -> Result<()> {
    require_nextest();
    let Some(server) = common::spawn_configured_nats_server("max_payload: 65536\n").await? else {
        return Ok(());
    };
    let js = local_test_nats(server.url()).await?;
    let (store, _) = seed_session_metadata(&js, "retention-failure").await?;
    harnx_blob_store::create_or_open_attachments_bucket(&js, 1).await?;
    let stream = js.get_stream("OBJ_harnx_attachments").await?;
    let mut sealed = stream.cached_info().config.clone();
    sealed.sealed = true;
    js.update_stream(sealed).await?;
    let key = storage_key("retention-failure");
    let backend =
        NatsSessionLogBackend::new(js.clone(), key.clone(), 1).with_metadata_store(Some(store));
    let log = NatsSessionLog::new_with_replicas(js, key, 1);
    let original = output("call", 70_000);
    let seq = backend.append_event(&entry(vec![original.clone()])).await?;
    let actual = appended(&log, seq).await?;
    let expected = entry(vec![with_error(
        &original,
        full_size_error_message(serde_json::to_vec(&original)?.len(), None),
    )]);
    assert_eq!(actual, expected);
    assert!(size(&actual) <= BUDGET);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retained_reference_cannot_expand_a_boundary_output() -> Result<()> {
    require_nextest();
    let Some(server) = common::spawn_configured_nats_server("max_payload: 65536\n").await? else {
        return Ok(());
    };
    let js = local_test_nats(server.url()).await?;
    let (store, _) = seed_session_metadata(&js, "boundary").await?;
    let owner = SessionRef::new(None, "boundary".into())?;
    let original = (0..512)
        .map(|n| output("boundary", n))
        .find(|original| {
            let original_size = size(&entry(vec![original.clone()]));
            let lower = with_error(original, full_size_error_message(0, None));
            let actual = with_error(
                original,
                full_size_error_message(
                    serde_json::to_vec(original).unwrap().len(),
                    Some(&retained_url(&owner, original)),
                ),
            );
            size(&entry(vec![lower])) < original_size && size(&entry(vec![actual])) > original_size
        })
        .context("boundary smaller than its retained replacement")?;
    let mut padding = output("p", 0);
    let len = size(&entry(vec![original.clone(), padding.clone()]));
    padding.id = Some("p".repeat(BUDGET + 1 - len + 1));
    let input = entry(vec![original.clone(), padding.clone()]);
    assert_eq!(size(&input), BUDGET + 1);
    let url = retained_url(&owner, &original);
    let expected = entry(vec![
        with_error(&original, short_size_error_message(&url)),
        padding,
    ]);
    assert!(size(&expected) <= BUDGET);
    let key = storage_key("boundary");
    let backend =
        NatsSessionLogBackend::new(js.clone(), key.clone(), 1).with_metadata_store(Some(store));
    let log = NatsSessionLog::new_with_replicas(js, key, 1);
    let seq = backend.append_event(&input).await?;
    assert_eq!(
        appended(&log, seq).await?,
        expected,
        "reject the expanding full error, but still try its shorter fallback"
    );
    Ok(())
}

async fn retained_fallback(minimal: bool) -> Result<()> {
    require_nextest();
    let Some(server) = common::spawn_configured_nats_server("max_payload: 65536\n").await? else {
        return Ok(());
    };
    let js = local_test_nats(server.url()).await?;
    let (store, _) = seed_session_metadata(&js, "fallback").await?;
    let owner = SessionRef::new(None, "fallback".into())?;
    let mut original = output("p", 4096);
    // Spend the budget on immutable identity, leaving room only for the desired
    // message. CID length is fixed even though padding changes its hash.
    let url = retained_url(&owner, &original);
    let message = if minimal {
        minimal_size_error_message(true)
    } else {
        short_size_error_message(&url)
    };
    let len = size(&entry(vec![with_error(&original, message)]));
    original.id = Some("p".repeat(BUDGET - len + 1));
    let url = retained_url(&owner, &original);
    let message = if minimal {
        minimal_size_error_message(true)
    } else {
        short_size_error_message(&url)
    };
    let expected = entry(vec![with_error(&original, message)]);
    assert_eq!(size(&expected), BUDGET);
    assert!(
        size(&entry(vec![with_error(
            &original,
            full_size_error_message(serde_json::to_vec(&original)?.len(), Some(&url))
        )])) > BUDGET
    );
    if minimal {
        assert!(
            size(&entry(vec![with_error(
                &original,
                short_size_error_message(&url)
            )])) > BUDGET
        );
    }
    let key = storage_key("fallback");
    let backend =
        NatsSessionLogBackend::new(js.clone(), key.clone(), 1).with_metadata_store(Some(store));
    let log = NatsSessionLog::new_with_replicas(js.clone(), key, 1);
    let seq = backend.append_event(&entry(vec![original.clone()])).await?;
    assert_eq!(appended(&log, seq).await?, expected);
    let (bytes, _) = harnx_runtime::nats_attachments::get_session_attachment(&js, 1, &url)
        .await?
        .context("original retained even when its URL does not fit")?;
    assert_eq!(serde_json::from_slice::<ToolOutput>(&bytes)?, original);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retained_pass_two_keeps_a_readable_reference() -> Result<()> {
    retained_fallback(false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retained_last_resort_keeps_an_honest_retention_claim() -> Result<()> {
    retained_fallback(true).await
}
