use super::tests::{
    env_lock, seed_remote_config, spawn_metis_worker_with_call_fn, spawn_test_nats,
    subagent_test_env, test_subagent_toolset,
};
use crate::nats_session::test_support::InheritedTestTool;
use harnx_core::cid_url::{CidUrl, PlanItem, SessionRef};
use harnx_core::message::{ImageUrl, MessageContent, MessageContentPart, MessageRole};
use harnx_toolset::ToolInvokeError;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

fn capture_prompt_content(
    captured: Arc<Mutex<Vec<MessageContent>>>,
) -> crate::agent_loop::AgentCallFn {
    Arc::new(move |input, config, _abort| {
        let captured = Arc::clone(&captured);
        Box::pin(async move {
            let messages = crate::config::input::build_messages(input, config)?;
            let content = messages
                .iter()
                .rev()
                .find(|message| message.role == MessageRole::User)
                .expect("child request includes user prompt")
                .content
                .clone();
            captured.lock().await.push(content);
            Ok((
                "attachment received".to_string(),
                None,
                vec![],
                crate::client::CompletionTokenUsage::default(),
            ))
        })
    })
}

async fn assert_image_handoff(
    toolset: &super::subagent_toolset::SubagentToolset,
    captured: &Mutex<Vec<MessageContent>>,
    image_url: &str,
) {
    toolset
        .invoke_inherited(
            "session_prompt",
            json!({"message": "inspect image", "attachments": [image_url]}),
            CancellationToken::new(),
        )
        .await
        .expect("image prompt succeeds");
    assert_eq!(
        captured.lock().await.last(),
        Some(&MessageContent::Array(vec![
            MessageContentPart::Text {
                text: "inspect image".to_string(),
            },
            MessageContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: image_url.to_string(),
                },
            },
        ]))
    );
}

async fn assert_text_and_plan_handoff(
    toolset: &super::subagent_toolset::SubagentToolset,
    captured: &Mutex<Vec<MessageContent>>,
    text_url: &str,
    plan_url: &str,
) {
    toolset
        .invoke_inherited(
            "session_prompt",
            json!({
                "message": "inspect references",
                "attachments": [text_url, plan_url],
            }),
            CancellationToken::new(),
        )
        .await
        .expect("reference prompt succeeds");
    assert_eq!(
        captured.lock().await.last(),
        Some(&MessageContent::Text(format!(
            "inspect references\nAttachment: {text_url}\nAttachment: {plan_url}"
        )))
    );
}

async fn assert_invalid_url_does_not_spawn(
    toolset: &super::subagent_toolset::SubagentToolset,
    metadata: &crate::nats_session_metadata::SessionMetadataStore,
) {
    let before = metadata.list().await.expect("list child sessions").len();
    let error = toolset
        .invoke_inherited(
            "session_prompt",
            json!({"message": "bad handoff", "attachments": ["https://example.com/a.png"]}),
            CancellationToken::new(),
        )
        .await
        .expect_err("invalid URL must fail");
    let ToolInvokeError::Recoverable(message) = error else {
        panic!("invalid attachment URL must be recoverable");
    };
    assert!(message.contains("invalid attachment URL 'https://example.com/a.png':"));
    assert_eq!(
        metadata.list().await.expect("list child sessions").len(),
        before,
        "invalid attachments must fail before child creation"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_prompt_hands_off_image_text_and_plan_attachments() {
    let _env_guard = env_lock().await;
    let Some((url, mut nats, _store_dir)) = spawn_test_nats().await else {
        return;
    };
    let seeded = seed_remote_config(&url);
    let _env = subagent_test_env(&url, &seeded);
    let captured = Arc::new(Mutex::new(Vec::new()));
    let daemon = spawn_metis_worker_with_call_fn(&url, capture_prompt_content(captured.clone()));
    let toolset = test_subagent_toolset(&url).await;
    let client = async_nats::connect(&url).await.unwrap();
    let jetstream = async_nats::jetstream::new(client);
    let metadata = crate::nats_session_metadata::SessionMetadataStore::ensure(&jetstream, 1)
        .await
        .unwrap();
    let session = SessionRef::new(Some("source".to_string()), "source1".to_string()).unwrap();
    let image_hash = harnx_core::crypto::sha256("data:image/png;base64,UE5H");
    let image_url = harnx_blob_store::media_cid_url(&session, &image_hash);
    let text_url = harnx_blob_store::media_cid_url(&session, &harnx_core::crypto::sha256("notes"));
    let plan_url = CidUrl::Plan {
        session,
        slug: "handoff".to_string(),
        item: PlanItem::Index,
    };
    let store = harnx_blob_store::media::ensure_attachments_bucket(&jetstream, 1)
        .await
        .unwrap();
    harnx_blob_store::media::put_media(&store, &image_url, b"PNG", "image/png")
        .await
        .unwrap();
    harnx_blob_store::media::put_media(&store, &text_url, b"notes", "text/plain")
        .await
        .unwrap();

    assert_image_handoff(&toolset, &captured, &image_url.to_string()).await;
    assert_text_and_plan_handoff(
        &toolset,
        &captured,
        &text_url.to_string(),
        &plan_url.to_string(),
    )
    .await;
    assert_invalid_url_does_not_spawn(&toolset, &metadata).await;

    daemon.abort();
    let _ = daemon.await;
    let _ = nats.kill();
    let _ = nats.wait();
}
