use super::*;
use crate::operator_tools::OperatorToolMessage;

/// #2655 regression (CodeRabbit review 5830157431): a worker that cannot
/// deserialize an operator command must answer promptly instead of leaving
/// the frontend to hit its 60-second acknowledgement timeout. The payload is
/// published raw, bypassing `OperatorToolRequest`, so an `action` the
/// worker's enum does not know models a newer command meeting an older
/// worker — the same parse failure the current worker hits at its own
/// `from_slice` boundary.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_worker_replies_promptly_with_finished_error_for_unknown_action() -> Result<()> {
    let _lock = env_lock().await;
    let Some((url, mut nats, _store)) = spawn_test_nats().await else {
        return Ok(());
    };
    let seeded = seed_remote_config(&url);
    let _env = subagent_test_env(&url, &seeded);
    std::fs::write(
        seeded.config_dir().join("agents/metis.md"),
        "---\nmodel: test:test-model\n---\nfixture agent\n",
    )?;
    let scope = ServerScope::new();
    let _scope = TestEnvGuard::new("HARNX_SERVER_SCOPE", scope.as_str());
    let client = async_nats::connect(&url).await?;
    let fixture = LiveOperatorFixture::start(
        seeded.parent_config.clone(),
        client,
        scope,
        SessionIdentity {
            id: "unknown-action",
            agent: "metis",
        },
    )
    .await?;
    fixture.wait_ready().await?;
    let inbox = fixture.client.new_inbox();
    let mut replies = fixture.client.subscribe(inbox.clone()).await?;
    fixture.client.flush().await?;
    fixture
        .client
        .publish_with_reply(
            "cluster.local.operator_tools",
            inbox,
            serde_json::to_vec(&json!({
                "version": 1,
                "session_key": fixture.request.session_key,
                "use_tools": null,
                "tool_use": true,
                "command": {"action": "future_action"},
                "json": true,
                "bash_timeout_secs": null,
            }))?
            .into(),
        )
        .await?;
    assert_unknown_action_reply(&mut replies, &fixture).await?;
    fixture.close().await?;
    nats.kill()?;
    nats.wait()?;
    Ok(())
}

async fn assert_unknown_action_reply(
    replies: &mut async_nats::Subscriber,
    fixture: &LiveOperatorFixture,
) -> Result<()> {
    let reply = tokio::time::timeout(Duration::from_secs(5), replies.next())
        .await
        .context("worker did not reply promptly to an unknown command")?
        .context("reply channel closed")?;
    let message: OperatorToolMessage = serde_json::from_slice(&reply.payload)?;
    let OperatorToolMessage::Finished(finished) = message else {
        panic!("unknown action must produce a Finished error, not {message:?}");
    };
    assert_eq!(finished.output, "", "{finished:?}");
    assert!(
        finished
            .error
            .as_deref()
            .is_some_and(|error| error.contains("unknown variant `future_action`")),
        "unknown action must surface the worker's serde error: {finished:?}"
    );
    assert_eq!(
        fixture.model_calls.load(Ordering::SeqCst),
        0,
        "an unparseable command must not create an inference turn"
    );
    Ok(())
}
