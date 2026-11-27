use super::harness::{extract_tool_results, single_call_model, Harness, AGENT_NAME, SESSION_ID};
use crate::worker::CI_SAFE_TIMEOUT;
use anyhow::{Context, Result};
use harnx_core::{
    attachments::{cid_for_data_url, cid_for_data_url_with_session, read_attachment_async},
    crypto::base64_decode,
    event::NullSink,
    message::MessageContentPart,
};
use harnx_runtime::{
    config::{Config, SessionAttachmentPath},
    nats_session_log::NatsSessionLog,
};
use serde_json::json;
use std::sync::Arc;

const PNG_BASE64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+ip1sAAAAASUVORK5CYII=";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inline_tool_image_survives_durable_round_before_cache_hydration() -> Result<()> {
    let middleware = format!(
        r#"middleware:
- name: return-image
  match: '^probe_run$'
  python: |
    call_next({{'inline_image_data': '{PNG_BASE64}'}})
"#
    );
    let model = single_call_model("probe_run", json!({}), "inline-image");
    let fixture = Harness::required_start(&middleware, model).await?;
    let cache = Config::session_attachments_dir(SessionAttachmentPath {
        agent_name: AGENT_NAME,
        session_id: SESSION_ID,
    })
    .context("worker cache path")?;
    assert!(!cache.exists());

    // The checkpoint uploads the inline image. The real worker must sync
    // that CID at the tool-round boundary, before the next model call hydrates it.
    let result = tokio::time::timeout(
        CI_SAFE_TIMEOUT,
        fixture.session.clone().with_external_admission().run_turn(
            "Return an image",
            Arc::new(NullSink),
            None,
        ),
    )
    .await??;
    assert!(
        result.error.is_none(),
        "worker turn failed: {:?}",
        result.error
    );
    assert_eq!(result.response.as_deref(), Some("activation completed"));
    fixture.probe.assert_single_call("probe_run");

    let data_url = format!("data:image/png;base64,{PNG_BASE64}");
    let cid = cid_for_data_url_with_session(&fixture.session.session_ref(), &data_url);
    let log = NatsSessionLog::new(fixture.js.clone(), fixture.session.storage_key());
    let entries = log.load_events_latest_required_async().await?;
    let results = extract_tool_results(&entries);
    assert_eq!(results.len(), 1);
    assert!(
        results[0].content.iter().any(|part| matches!(part,
            MessageContentPart::ImageUrl { image_url } if image_url.url == cid
        )),
        "durable tool result must reference the uploaded image: {results:?}"
    );
    assert_eq!(
        read_attachment_async(&cache, &cid_for_data_url(&data_url)).await?,
        (base64_decode(PNG_BASE64)?, "image/png".into())
    );
    Ok(())
}
