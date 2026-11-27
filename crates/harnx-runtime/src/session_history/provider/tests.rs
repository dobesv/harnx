use super::*;
use crate::config::{Config, ConfigLock, NatsRouting};
use crate::nats_session_log::NatsSessionLog;
use crate::nats_session_metadata::{SessionInitializer, SessionMetadata, SessionMetadataStore};
use anyhow::Context;
use harnx_core::abort::create_abort_signal;
use harnx_core::message::{MessageContent, MessageRole};
use harnx_core::session::Session;

async fn provider_with_history(url: &str) -> anyhow::Result<SessionHistoryProvider> {
    let session = Session {
        id: "hist01".into(),
        ..Default::default()
    };
    let config = Config {
        nats_routing: NatsRouting::Cluster("history-test".into()),
        nats_servers: vec![serde_json::from_value(
            json!({"name": "history-test", "url": url}),
        )?],
        session: Some(session),
        ..Default::default()
    };
    let metadata = SessionMetadata::new(
        "hist01",
        SessionInitializer::inline("test", Default::default(), Default::default()),
    );
    let js = config.nats_jetstream("history-test").await?;
    let store = SessionMetadataStore::ensure(&js, 1).await?;
    store
        .create(&metadata)
        .await?
        .context("create test session")?;
    NatsSessionLog::new_with_replicas(js, metadata.storage_key(), 1)
        .append_event_async(&SessionLogEntry::Message {
            id: None,
            role: MessageRole::User,
            content: MessageContent::Text("seeded history".into()),
            timestamp: None,
            fence_token: None,
        })
        .await?;
    Ok(SessionHistoryProvider::new(Arc::new(ConfigLock::new(
        config,
    ))))
}

async fn call(
    provider: &SessionHistoryProvider,
    arguments: Value,
    structured_result: bool,
) -> Result<ToolProviderOutput, ToolError> {
    provider
        .call_tool_with_progress(
            TOOL_NAME,
            arguments,
            ToolCallOrigin {
                structured_result,
                ..Default::default()
            },
            &create_abort_signal(),
            Arc::new(NoopToolProgress),
        )
        .await
}

#[tokio::test]
async fn history_provider_preserves_text_and_opts_into_structured_values() -> anyhow::Result<()> {
    let broker = crate::nats_test_common::spawn_nats_server()
        .await?
        .context("nats-server required")?;
    let provider = provider_with_history(broker.url()).await?;
    let rows = json!([{"seq": 1, "type": "message", "text": "seeded history", "role": "user"}]);
    for (args, payload) in [
        (json!({}), rows),
        (json!({"jaq": "{count: length}"}), json!({"count": 1})),
        (json!({"jaq": ".[0].text"}), json!("seeded history")),
        (json!({"jaq": "null"}), Value::Null),
    ] {
        let legacy = call(&provider, args.clone(), false)
            .await
            .ok()
            .expect("text result");
        let structured = call(&provider, args, true)
            .await
            .ok()
            .expect("structured result");
        assert_eq!(
            legacy.value,
            json!({"content": [{"type": "text", "text": payload.to_string()}]})
        );
        assert_eq!(structured.value["content"], legacy.value["content"]);
        assert_eq!(structured.value.get("structuredContent"), Some(&payload));
    }
    for structured_result in [false, true] {
        assert!(matches!(
            call(&provider, json!({"jaq": "["}), structured_result).await,
            Err(ToolError::Recoverable(_))
        ));
    }
    Ok(())
}
