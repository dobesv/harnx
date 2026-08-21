use super::*;

async fn append_completed_transcript_fixture(config: &Config, session_id: &str) {
    let jetstream = config
        .nats_jetstream("local")
        .await
        .expect("load local jetstream");
    let log = NatsSessionLog::new(jetstream, session_id.to_string());
    let header = crate::config::session::new(config, session_id, None)
        .expect("build fixture session")
        .build_header_entry();
    log.append_event_async(&header)
        .await
        .expect("append fixture header");
    log.append_event_async(&SessionLogEntry::Message {
        id: Some(uuid::Uuid::new_v4().to_string()),
        role: harnx_core::message::MessageRole::User,
        content: harnx_core::message::MessageContent::Text("delegate over nats".to_string()),
        timestamp: None,
        fence_token: None,
    })
    .await
    .expect("append fixture user message");
    log.append_event_async(&SessionLogEntry::Message {
        id: Some(uuid::Uuid::new_v4().to_string()),
        role: harnx_core::message::MessageRole::Assistant,
        content: harnx_core::message::MessageContent::Text(
            "stub remote reply over nats".to_string(),
        ),
        timestamp: None,
        fence_token: None,
    })
    .await
    .expect("append fixture assistant message");
    log.append_event_async(&SessionLogEntry::TurnEnd {
        through_seq: 2,
        fence_token: 1,
        timestamp: None,
    })
    .await
    .expect("append fixture turn end");
}

async fn append_compressed_tool_fixture(log: &NatsSessionLog) {
    let tool_call = ToolCall::new(
        "fs_read".to_string(),
        json!({"path": "README.md"}),
        Some("tool-1".to_string()),
        None,
    );
    log.append_event_async(&SessionLogEntry::ToolCalls {
        text: "calling tool".to_string(),
        thought: Some("tool thought".to_string()),
        calls: vec![tool_call.clone()],
        timestamp: None,
        fence_token: None,
    })
    .await
    .expect("append tool calls");
    log.append_event_async(&SessionLogEntry::ToolResults {
        results: vec![ToolOutput {
            id: tool_call.id.clone(),
            name: tool_call.name.clone(),
            output: json!({"ok": true}),
            markdown: None,
            content: vec![],
            switch_agent: None,
        }],
        timestamp: None,
    })
    .await
    .expect("append tool results");
    log.append_event_async(&SessionLogEntry::Compress {
        prompt: "summary prompt".to_string(),
    })
    .await
    .expect("append compress");
    log.append_event_async(&SessionLogEntry::Message {
        id: Some(uuid::Uuid::new_v4().to_string()),
        role: harnx_core::message::MessageRole::User,
        content: harnx_core::message::MessageContent::Text("after compress user".to_string()),
        timestamp: None,
        fence_token: None,
    })
    .await
    .expect("append post-compress user");
}

fn assert_compressed_tool_transcript(
    transcript: &crate::config::remote_session_ops::RemoteTranscriptState,
) {
    assert!(!transcript.compressed_messages.is_empty());
    assert!(transcript
        .compressed_messages
        .iter()
        .any(|message| message.role == harnx_core::message::MessageRole::Tool));
    assert!(transcript
        .messages
        .iter()
        .all(|message| message.role != harnx_core::message::MessageRole::System));
    assert_eq!(
        transcript.compaction_summary.as_deref(),
        Some("summary prompt")
    );
    let active_rows: Vec<_> = transcript
        .messages
        .iter()
        .map(|message| (message.role, message.content.to_text(), message.log_seq))
        .collect();
    assert_eq!(
        active_rows,
        vec![(
            harnx_core::message::MessageRole::User,
            "after compress user".to_string(),
            Some(0),
        )]
    );
}

async fn activate_nats_session(
    seeded: &mut SeededRemoteParentConfig,
    session_id: String,
) -> NatsSession {
    seeded
        .parent_config
        .set_remote_agent("metis".to_string(), "local".to_string());
    seeded
        .parent_config
        .use_session(Some(&session_id))
        .expect("activate remote session id");
    let global_config = Arc::new(parking_lot::RwLock::new(std::mem::take(
        &mut seeded.parent_config,
    )));
    NatsSession::from_global_config(
        crate::NatsSessionConfig {
            cluster: "local".to_string(),
            agent: "metis".to_string(),
            session_id: Some(session_id),
            activation_route: crate::SessionActivationRoute::ClusterShared,
        },
        &global_config,
        harnx_core::abort::create_abort_signal(),
    )
    .await
    .expect("load session")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn load_remote_transcript_for_render_prerenders_logical_rows() {
    let _env_guard = env_lock().await;
    let Some((url, mut child, _store_dir)) = spawn_test_nats().await else {
        return;
    };

    let mut seeded = seed_remote_config(&url);
    let session_id = crate::nats_worker::new_remote_session_id();
    append_completed_transcript_fixture(&seeded.parent_config, &session_id).await;

    let session = activate_nats_session(&mut seeded, session_id).await;

    let transcript = load_remote_transcript_for_render(&session)
        .await
        .expect("load transcript state");
    assert!(transcript.compressed_messages.is_empty());
    let row_seqs: Vec<usize> = transcript
        .messages
        .iter()
        .filter_map(|message| message.log_seq)
        .collect();
    assert_eq!(row_seqs, vec![1, 2]);
    let row_texts: Vec<_> = transcript
        .messages
        .iter()
        .map(|message| (message.role, message.content.to_text()))
        .collect();
    assert_eq!(
        row_texts,
        vec![
            (
                harnx_core::message::MessageRole::User,
                "delegate over nats".to_string(),
            ),
            (
                harnx_core::message::MessageRole::Assistant,
                "stub remote reply over nats".to_string(),
            ),
        ]
    );

    let _ = child.kill();
    let _ = child.wait();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn load_remote_transcript_for_render_keeps_tool_rows_and_compressed_prefix() {
    let _env_guard = env_lock().await;
    let Some((url, mut child, _store_dir)) = spawn_test_nats().await else {
        return;
    };

    let mut seeded = seed_remote_config(&url);
    let session_id = crate::nats_worker::new_remote_session_id();
    append_completed_transcript_fixture(&seeded.parent_config, &session_id).await;

    let jetstream = seeded
        .parent_config
        .nats_jetstream("local")
        .await
        .expect("load local jetstream");
    let log = NatsSessionLog::new(jetstream, session_id.clone());
    append_compressed_tool_fixture(&log).await;

    let session = activate_nats_session(&mut seeded, session_id).await;

    let transcript = load_remote_transcript_for_render(&session)
        .await
        .expect("load transcript state");
    assert_compressed_tool_transcript(&transcript);

    let _ = child.kill();
    let _ = child.wait();
}
