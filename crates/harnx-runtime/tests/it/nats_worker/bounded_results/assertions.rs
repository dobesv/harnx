use super::*;
use harnx_core::cid_url::CidUrl;
use harnx_toolset::ToolReply;

pub(super) fn tool_output(request: &Value, id: &str) -> Result<Value> {
    let message = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool" && message["tool_call_id"] == id)
        .with_context(|| format!("missing provider tool result {id}"))?;
    Ok(serde_json::from_str(
        message["content"].as_str().context("tool text")?,
    )?)
}

pub(super) async fn assert_bridge_result(
    js: &async_nats::jetstream::Context,
    bridge: &Value,
) -> Result<()> {
    assert!(serde_json::to_vec(bridge)?.len() <= BRIDGE_CAP);
    assert_eq!(bridge["isError"], false);
    assert_eq!(
        bridge["content"][1],
        json!({"type": "text", "text": "fitting MCP part"})
    );
    let preview = bridge["content"][0]["text"].as_str().unwrap();
    assert!(preview.contains("bridge START") && preview.contains("bridge END"));
    assert_eq!(
        harnx_runtime::nats_attachments::get_session_attachment(js, 1, attachment_url(preview))
            .await?,
        Some((bridge_text().into_bytes(), "text/plain".into()))
    );
    Ok(())
}

pub(super) async fn assert_native_result(
    js: &async_nats::jetstream::Context,
    native: &Value,
) -> Result<()> {
    assert_eq!(native["is_error"], true);
    let error = native["error"].as_str().unwrap();
    assert!(error.contains("size limit"));
    assert!(serde_json::to_vec(native)?.len() < 2048);
    let (original, mime) =
        harnx_runtime::nats_attachments::get_session_attachment(js, 1, attachment_url(error))
            .await?
            .context("native original")?;
    assert_eq!(mime, "application/json");
    let original: ToolReply = serde_json::from_slice(&original)?;
    assert_eq!(original.result.unwrap(), native_output("native_large"));
    Ok(())
}

pub(super) fn assert_provider_context(next: &Value, bridge: &Value, native: &Value) -> Result<()> {
    // These are provider-bound context and whole request bounds, not transport envelopes.
    let context_bytes = serde_json::to_vec(&next["messages"])?.len();
    let request_bytes = serde_json::to_vec(next)?.len();
    assert!(
        context_bytes < BRIDGE_CAP + SIBLING_BYTES + 8192,
        "context={context_bytes}"
    );
    assert!(
        request_bytes < BRIDGE_CAP + SIBLING_BYTES + 16384,
        "request={request_bytes}"
    );
    assert!(
        !next.to_string().contains(&"n".repeat(4096)),
        "native original leaked into context"
    );
    assert!(!next.to_string().contains("attachment unavailable"));
    eprintln!(
        "provider context={context_bytes} request={request_bytes}; bridge={} native={} sibling={}",
        serde_json::to_vec(bridge)?.len(),
        serde_json::to_vec(native)?.len(),
        serde_json::to_vec(&native_output("native_sibling"))?.len()
    );
    Ok(())
}

pub(super) async fn assert_durable_completions(
    js: &async_nats::jetstream::Context,
    session_key: &str,
) -> Result<()> {
    let rows = InvocationJournal::ensure(js, 1)
        .await?
        .records_for_session(session_key)
        .await?;
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|row| row.reply.is_some()));
    let native_row = rows
        .iter()
        .find(|row| row.request.tool_call_id.as_deref() == Some("native-large"))
        .unwrap();
    assert!(matches!(
        native_row.reply.as_ref().unwrap().result,
        Err(ToolErrorPayload::Recoverable(_))
    ));
    Ok(())
}

fn attachment_url(text: &str) -> &str {
    let start = text
        .find("cid:media:")
        .expect("retained original reference");
    let tail = &text[start..];
    let url = tail
        .split_whitespace()
        .next()
        .unwrap()
        .trim_end_matches(['.', ')', ']', '>']);
    CidUrl::parse(url).unwrap();
    url
}
