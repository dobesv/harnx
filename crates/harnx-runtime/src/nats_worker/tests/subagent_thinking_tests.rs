use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subagent_results_strip_thinking_but_preserve_child_log() {
    const INITIAL_REPLY: &str = "<think>child bootstrap reasoning</think>\nSession ready.";
    let _env_guard = env_lock().await;
    let (url, _nats, _store_dir) = spawn_test_nats().await.expect("nats-server required");
    let seeded = seed_remote_config(&url);
    let _env = subagent_test_env(&url, &seeded);
    let call_fn = bootstrap_then_echo_call_fn(INITIAL_REPLY);
    let daemon = spawn_metis_worker_with_call_fn(&url, call_fn);
    let toolset = test_subagent_toolset(&url).await;
    let created = tokio::time::timeout(
        NATS_TEST_CONDITION_TIMEOUT,
        toolset.invoke_inherited("session_new", json!({}), CancellationToken::new()),
    )
    .await
    .expect("session_new timed out")
    .expect("session_new succeeded");
    assert_eq!(created["response"], "Session ready.");
    let session_id = created["session_id"].as_str().expect("child session id");
    let mut raw_replies = vec![INITIAL_REPLY.to_owned()];
    for (raw, expected) in [
        (
            "<think>child reasoning</think>\nFinal answer.",
            "Final answer.",
        ),
        (
            " \n<think>multiline\nchild reasoning</think>\n\nFinal answer.\n",
            "Final answer.\n",
        ),
        ("  Ordinary response.\n", "  Ordinary response.\n"),
        ("<think>thinking only</think>\n", ""),
        (
            "Example: `<think>literal tag</think>`.",
            "Example: `<think>literal tag</think>`.",
        ),
    ] {
        let result = tokio::time::timeout(
            NATS_TEST_CONDITION_TIMEOUT,
            toolset.invoke_inherited(
                "session_prompt",
                json!({ "message": raw, "session_id": session_id }),
                CancellationToken::new(),
            ),
        )
        .await
        .expect("session_prompt timed out")
        .expect("session_prompt succeeded");
        assert_eq!(result["response"], expected, "raw reply: {raw:?}");
        assert_eq!(result["session_id"], session_id);
        assert_eq!(result["sub_agent"], created["sub_agent"]);
        assert_eq!(result["sub_agent_progress"]["agent"], "metis");
        assert_eq!(result["sub_agent_progress"]["session_id"], session_id);
        assert_eq!(result["sub_agent_progress"]["status"], "done");
        assert!(result.get("termination").is_none());
        raw_replies.push(raw.to_owned());
    }
    assert_raw_child_log(&seeded.parent_config, session_id, &raw_replies).await;

    daemon.abort();
    let _ = daemon.await;
}

fn bootstrap_then_echo_call_fn(initial_reply: &'static str) -> crate::agent_loop::AgentCallFn {
    Arc::new(move |input, _config, _abort| {
        let response = match input.text().as_str() {
            "Start a new session." => initial_reply.to_owned(),
            _ => input.text(),
        };
        Box::pin(async move {
            Ok((
                response,
                None,
                vec![],
                crate::client::CompletionTokenUsage::default(),
            ))
        })
    })
}

async fn assert_raw_child_log(config: &Config, session_id: &str, raw_replies: &[String]) {
    let log = NatsSessionLog::for_agent(
        config.nats_jetstream("local").await.unwrap(),
        "metis",
        session_id,
    )
    .with_replicas(1);
    let entries = log.load_events_async().await.expect("load child log");
    let persisted_replies: Vec<_> = entries
        .iter()
        .filter_map(|(_, entry)| match entry {
            SessionLogEntry::Message { role, content, .. } if role.is_assistant() => {
                Some(content.to_text())
            }
            _ => None,
        })
        .collect();
    assert_eq!(persisted_replies, raw_replies, "child log stays raw");
}
