use anyhow::Result;
#[cfg(unix)]
use base64::{engine::general_purpose::STANDARD, Engine};
#[cfg(unix)]
use harnx_core::attachments::cid_for_data_url_with_session;
use harnx_core::cid_url::SessionRef;
use harnx_mcp_bridge::result_limits::{
    parse_max_result_bytes, ResultLimits, DEFAULT_MAX_RESULT_BYTES,
};
use serde_json::{json, Value};

fn len(value: &Value) -> usize {
    serde_json::to_vec(value).unwrap().len()
}

fn session() -> SessionRef {
    SessionRef::new(Some("bridge/test".into()), "Abc123".into()).unwrap()
}

#[cfg(unix)]
fn cid(mime: &str, bytes: &[u8]) -> String {
    cid_for_data_url_with_session(
        &session(),
        &format!("data:{mime};base64,{}", STANDARD.encode(bytes)),
    )
}

fn text(value: &str) -> Value {
    json!({"content": [{"type": "text", "text": value}]})
}

fn bounded(result: &Value, cap: usize) {
    assert!(len(result) <= cap, "{} > {cap}", len(result));
    serde_json::from_value::<rmcp::model::CallToolResult>(result.clone())
        .expect("valid MCP result");
}

#[tokio::test]
async fn below_exact_above_default_cap() -> Result<()> {
    let limits = ResultLimits::default();
    let overhead = len(&text(""));
    for size in [DEFAULT_MAX_RESULT_BYTES - 1, DEFAULT_MAX_RESULT_BYTES] {
        let original = text(&"x".repeat(size - overhead));
        assert_eq!(len(&original), size);
        assert_eq!(limits.normalize(original.clone(), None).await?, original);
    }
    let original = text(&"x".repeat(DEFAULT_MAX_RESULT_BYTES + 1 - overhead));
    assert_eq!(len(&original), DEFAULT_MAX_RESULT_BYTES + 1);
    let result = limits.normalize(original, None).await?;
    bounded(&result, DEFAULT_MAX_RESULT_BYTES);
    assert_eq!(result["isError"], true);
    assert!(result["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("original was not retained"));
    assert!(!result.to_string().contains("cid:"));
    Ok(())
}

#[tokio::test]
async fn fitting_fields_and_escaping_pass_unchanged() -> Result<()> {
    let original = json!({"content": [{"type": "text", "text": "\u{0000}\n\\\"日本語👋", "annotations": {"audience": ["user"], "priority": 0.4}, "_meta": {"nested": true}}], "structuredContent": {"a": [1, 2]}, "_meta": {"x": "y"}, "isError": true});
    assert_eq!(
        ResultLimits::new(1024)?
            .normalize(original.clone(), None)
            .await?,
        original
    );
    Ok(())
}

#[test]
fn invalid_tiny_caps_are_rejected() {
    for cap in ["0", "1", "1023", "-1", "NaN", "184467440737095516160"] {
        assert!(parse_max_result_bytes(cap).is_err(), "{cap}");
    }
    assert!(ResultLimits::new(1023).is_err());
    assert_eq!(parse_max_result_bytes("1024").unwrap(), 1024);
}

#[tokio::test]
async fn session_without_storage_is_bounded_and_honest() -> Result<()> {
    let result = ResultLimits::new(1024)?
        .normalize(text(&"x".repeat(1025)), Some(&session()))
        .await?;
    bounded(&result, 1024);
    assert_eq!(result["isError"], true);
    assert!(result
        .to_string()
        .contains("side effects may already have occurred"));
    assert!(!result.to_string().contains("cid:"));
    Ok(())
}

#[cfg(unix)]
mod stored {
    use super::*;
    use crate::bridge_roundtrip::{spawn_nats_server, TOKEN};
    use anyhow::Context;
    use harnx_toolset::ToolInvokeError;
    use harnx_toolset::{ToolInvocation, ToolInvocationContext, Toolset};
    use tokio_util::sync::CancellationToken;

    async fn setup() -> Result<
        Option<(
            crate::bridge_roundtrip::NatsServerHandle,
            async_nats::jetstream::Context,
        )>,
    > {
        let Some(server) = spawn_nats_server().await? else {
            return Ok(None);
        };
        let client = async_nats::ConnectOptions::with_token(TOKEN.into())
            .connect(&server.url)
            .await?;
        Ok(Some((server, async_nats::jetstream::new(client))))
    }

    async fn stored(js: &async_nats::jetstream::Context, uri: &str) -> Result<(Vec<u8>, String)> {
        let store = js
            .get_object_store(harnx_blob_store::ATTACHMENTS_BUCKET)
            .await?;
        harnx_blob_store::get_media(&store, &harnx_core::cid_url::CidUrl::parse(uri)?)
            .await?
            .context("stored original missing")
    }

    mod mixed_parts;
    mod process;

    #[tokio::test]
    async fn mixed_parts_preserve_exact_originals_mime_and_siblings() -> Result<()> {
        let Some((_server, js)) = setup().await? else {
            return Ok(());
        };
        let fixture = mixed_parts::MixedParts::new();
        let limits = ResultLimits::new(DEFAULT_MAX_RESULT_BYTES)?.with_storage(js.clone(), 1);
        let result = limits
            .normalize(fixture.original.clone(), Some(&session()))
            .await?;
        bounded(&result, DEFAULT_MAX_RESULT_BYTES);
        fixture.assert_fields_preserved(&result);
        fixture.assert_binary_originals(&js, &result).await?;
        fixture.assert_text_originals(&js, &result).await?;
        Ok(())
    }

    #[tokio::test]
    async fn exact_plus_one_with_storage_and_escaped_envelope_bound() -> Result<()> {
        let Some((_server, js)) = setup().await? else {
            return Ok(());
        };
        for cap in [1024, DEFAULT_MAX_RESULT_BYTES] {
            let limits = ResultLimits::new(cap)?.with_storage(js.clone(), 1);
            let original = text(&"x".repeat(cap + 1 - len(&text(""))));
            let result = limits.normalize(original.clone(), Some(&session())).await?;
            bounded(&result, cap);
            assert!(result.get("isError").is_none());
            let uri = cid(
                "text/plain",
                original["content"][0]["text"].as_str().unwrap().as_bytes(),
            );
            assert_eq!(
                stored(&js, &uri).await?.0,
                original["content"][0]["text"].as_str().unwrap().as_bytes()
            );
            let escaped = text(&"\"\\\n\u{0001}👋".repeat(cap));
            let preview = limits.normalize(escaped, Some(&session())).await?;
            bounded(&preview, cap);
            assert!(preview["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains('👋'));
        }
        Ok(())
    }

    #[tokio::test]
    async fn many_small_parts_use_json_aggregate_preserving_status_and_metadata() -> Result<()> {
        let Some((_server, js)) = setup().await? else {
            return Ok(());
        };
        let original = json!({"content": vec![json!({"type": "text", "text": "x".repeat(100)}); 200], "structuredContent": {"exact": true}, "_meta": {"preserved": true}, "isError": false});
        let result = ResultLimits::new(1024)?
            .with_storage(js.clone(), 1)
            .normalize(original.clone(), Some(&session()))
            .await?;
        bounded(&result, 1024);
        assert_eq!(result["isError"], false);
        assert_eq!(result["structuredContent"], original["structuredContent"]);
        assert_eq!(result["_meta"], original["_meta"]);
        let bytes = serde_json::to_vec(&original)?;
        let uri = cid("application/json", &bytes);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains(&uri));
        assert_eq!(stored(&js, &uri).await?, (bytes, "application/json".into()));
        Ok(())
    }

    #[tokio::test]
    async fn individually_fitting_medium_parts_and_oversized_with_many_siblings_use_aggregate(
    ) -> Result<()> {
        let Some((_server, js)) = setup().await? else {
            return Ok(());
        };
        let medium_image = json!({"type": "image", "data": STANDARD.encode(vec![0; 15000]), "mimeType": "image/png"});
        let medium_text = json!({"type": "text", "text": "x".repeat(20000)});
        let large_image = json!({"type": "image", "data": STANDARD.encode(vec![0; 40000]), "mimeType": "image/png"});
        for parts in [
            vec![medium_image, medium_text.clone()],
            vec![large_image, medium_text.clone(), medium_text],
        ] {
            let original = json!({"content": parts, "isError": false});
            let bytes = serde_json::to_vec(&original)?;
            let result = ResultLimits::new(DEFAULT_MAX_RESULT_BYTES)?
                .with_storage(js.clone(), 1)
                .normalize(original, Some(&session()))
                .await?;
            bounded(&result, DEFAULT_MAX_RESULT_BYTES);
            assert_eq!(result["isError"], false);
            let uri = cid("application/json", &bytes);
            assert!(result["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains(&uri));
            assert_eq!(stored(&js, &uri).await?, (bytes, "application/json".into()));
        }
        Ok(())
    }

    #[tokio::test]
    async fn large_structured_metadata_and_part_annotations_get_explicit_error_and_full_json(
    ) -> Result<()> {
        let Some((_server, js)) = setup().await? else {
            return Ok(());
        };
        let limits = ResultLimits::new(1024)?.with_storage(js.clone(), 1);
        let mut cases = vec![text("small"); 3];
        cases[0]["structuredContent"] = json!({"schemaRequired": "x".repeat(3000)});
        cases[1]["_meta"] = json!({"large": "x".repeat(3000)});
        cases[2]["content"][0]["annotations"] = json!({"large": "x".repeat(3000)});
        for original in cases {
            let bytes = serde_json::to_vec(&original)?;
            let result = limits.normalize(original, Some(&session())).await?;
            bounded(&result, 1024);
            assert_eq!(result["isError"], true);
            assert!(result.get("structuredContent").is_none());
            assert!(result.get("_meta").is_none());
            let uri = cid("application/json", &bytes);
            assert!(result["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains(&uri));
            assert_eq!(stored(&js, &uri).await?, (bytes, "application/json".into()));
        }
        Ok(())
    }

    #[tokio::test]
    async fn invalid_base64_and_oversized_links_use_explicit_json_fallback() -> Result<()> {
        let Some((_server, js)) = setup().await? else {
            return Ok(());
        };
        for part in [
            json!({"type": "resource", "resource": {"uri": "file:///blob", "blob": STANDARD.encode(vec![0; 4000]), "_meta": {"nested": true}}, "_meta": {"outer": true}}),
            json!({"type": "image", "data": "!".repeat(3000), "mimeType": "image/png"}),
            json!({"type": "resource_link", "uri": "file:///huge", "name": "x".repeat(3000)}),
            json!({"type": "image", "data": "AA==", "mimeType": "x".repeat(1_100_000)}),
        ] {
            let original = json!({"content": [part]});
            let bytes = serde_json::to_vec(&original)?;
            let result = ResultLimits::new(1024)?
                .with_storage(js.clone(), 1)
                .normalize(original, Some(&session()))
                .await?;
            bounded(&result, 1024);
            assert_eq!(result["isError"], true);
            assert_eq!(
                stored(&js, &cid("application/json", &bytes)).await?.0,
                bytes
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn missing_owner_does_not_create_bucket_but_actual_storage_failure_is_fatal() -> Result<()>
    {
        let Some((_server, js)) = setup().await? else {
            return Ok(());
        };
        let limits = ResultLimits::new(1024)?.with_storage(js.clone(), 1);
        let oversized = text(&"x".repeat(3000));
        let result = limits.normalize(oversized.clone(), None).await?;
        bounded(&result, 1024);
        assert_eq!(result["isError"], true);
        assert!(js
            .get_object_store(harnx_blob_store::ATTACHMENTS_BUCKET)
            .await
            .is_err());
        limits.normalize(oversized, Some(&session())).await?;
        js.delete_stream("OBJ_harnx_attachments").await?;
        let result = limits
            .normalize(text(&"different".repeat(3000)), Some(&session()))
            .await;
        assert!(
            matches!(result, Err(ToolInvokeError::Fatal(message)) if message.contains("attachment storage failed"))
        );
        Ok(())
    }

    #[tokio::test]
    async fn bucket_initialization_outage_is_fatal_while_fitting_results_need_no_storage(
    ) -> Result<()> {
        let Some((server, _js)) = setup().await? else {
            return Ok(());
        };
        let client = async_nats::ConnectOptions::with_token(TOKEN.into())
            .connect(&server.url)
            .await?;
        // A valid connected broker with no responder for this JetStream API domain.
        let mut unavailable = async_nats::jetstream::with_prefix(client, "UNAVAILABLE.API");
        unavailable.set_timeout(std::time::Duration::from_millis(100));
        let limits = ResultLimits::new(1024)?.with_storage(unavailable, 1);
        let small = text("unchanged");
        assert_eq!(
            limits.normalize(small.clone(), Some(&session())).await?,
            small
        );
        let result = limits
            .normalize(text(&"x".repeat(3000)), Some(&session()))
            .await;
        assert!(
            matches!(result, Err(ToolInvokeError::Fatal(message)) if message.contains("attachment storage failed"))
        );
        Ok(())
    }

    #[tokio::test]
    async fn bridge_process_uses_configured_limit_and_serving_connection() -> Result<()> {
        use crate::bridge_roundtrip::wait_for_registration;
        use harnx_core::instance::ServerScope;
        use harnx_toolset::server_identity_token;

        let Some((server, js)) = setup().await? else {
            return Ok(());
        };
        let dir = tempfile::tempdir()?;
        let script = dir.path().join("fixture.json");
        let large = "x".repeat(40000);
        std::fs::write(
            &script,
            serde_json::to_vec(
                &json!({"tools": [{"name": "large"}], "call_result": text(&large), "responses": ["unused"]}),
            )?,
        )?;
        let scope = ServerScope::new();
        let mut child = process::start(&server.url, &scope, &script)?;
        let client = async_nats::ConnectOptions::with_token(TOKEN.into())
            .connect(&server.url)
            .await?;
        let identity = server_identity_token(None, "", "limits");
        wait_for_registration(&client, &scope, &identity).await?;
        let value = process::invoke(&client, &scope).await?;
        bounded(&value, 2048);
        assert_ne!(value["isError"], true);
        let uri = cid("text/plain", large.as_bytes());
        assert!(value["content"][0]["text"].as_str().unwrap().contains(&uri));
        assert_eq!(
            stored(&js, &uri).await?,
            (large.into_bytes(), "text/plain".into())
        );
        process::stop(&mut child).await
    }

    #[tokio::test]
    async fn bridge_context_offloads_but_contextless_invoke_cannot() -> Result<()> {
        let Some((_server, js)) = setup().await? else {
            return Ok(());
        };
        let original = text(&"x".repeat(40000));
        let dir = tempfile::tempdir()?;
        let script = dir.path().join("fixture.json");
        std::fs::write(
            &script,
            serde_json::to_vec(
                &json!({"tools": [{"name": "large"}], "call_result": original, "responses": ["unused"]}),
            )?,
        )?;
        let binary = std::path::Path::new(env!("CARGO_BIN_EXE_harnx-mcp-bridge"))
            .parent()
            .unwrap()
            .join(format!("harnx-mock-mcp{}", std::env::consts::EXE_SUFFIX));
        let mut bridge = harnx_mcp_bridge::BridgeToolset::new(
            "limits",
            vec![
                binary.to_string_lossy().into_owned(),
                "--script".into(),
                script.to_string_lossy().into_owned(),
            ],
        )
        .await?
        .with_result_limits(ResultLimits::new(1024)?.with_storage(js.clone(), 1));
        let result = bridge
            .invoke_with_context(ToolInvocation {
                tool: "large".into(),
                args: json!({}),
                context: ToolInvocationContext {
                    invoking_session: Some(harnx_toolset::SessionRef {
                        agent: session().agent,
                        session_id: session().session_id,
                    }),
                    ..Default::default()
                },
                cancel: CancellationToken::new(),
            })
            .await?;
        bounded(&result, 1024);
        assert_ne!(result["isError"], true);
        assert_eq!(
            stored(
                &js,
                &cid(
                    "text/plain",
                    original["content"][0]["text"].as_str().unwrap().as_bytes()
                )
            )
            .await?
            .0,
            vec![b'x'; 40000]
        );
        let result = bridge
            .invoke("large", json!({}), CancellationToken::new())
            .await?;
        assert_eq!(result["isError"], true);
        assert!(result.to_string().contains("original was not retained"));
        bridge.shutdown().await?;
        Ok(())
    }
}
