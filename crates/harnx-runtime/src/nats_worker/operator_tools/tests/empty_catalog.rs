use super::*;
use crate::operator_tools::BashToolSelection;

async fn run_session_bash(fixture: &LiveOperatorFixture, script: &str) -> OperatorToolReply {
    let mut request = fixture.request.clone();
    request.command = OperatorToolCommand::Bash {
        script: script.into(),
        selection: BashToolSelection::Session,
    };
    call(&fixture.client, &request, request.command.clone()).await
}

/// #2655 regression (CodeRabbit review 5830157431): pure shell must run with a
/// genuinely empty tool catalog. The metis-based matrix keeps a non-empty U,
/// and an empty inferred-name list does not empty that catalog; this fixture
/// agent has both `use_tools` and `bash_tools` empty, so session-mode Bash
/// admits no inner tool and the synthetic outer bash stays the whole catalog.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_worker_bash_empty_catalog_runs_pure_shell_without_inner_tools() -> Result<()> {
    let _lock = env_lock().await;
    let Some((url, mut nats, _store)) = spawn_test_nats().await else {
        return Ok(());
    };
    let seeded = seed_remote_config(&url);
    let _env = subagent_test_env(&url, &seeded);
    std::fs::write(
        seeded.config_dir().join("agents/empty_catalog.md"),
        "---\nmodel: test:test-model\nuse_tools: []\nbash_tools: []\n---\nempty catalog agent\n",
    )?;
    let scope = ServerScope::new();
    let _scope = TestEnvGuard::new("HARNX_SERVER_SCOPE", scope.as_str());
    let client = async_nats::connect(&url).await?;
    let fixture = LiveOperatorFixture::start(
        seeded.parent_config.clone(),
        client,
        scope,
        SessionIdentity {
            id: "empty-catalog-bash",
            agent: "empty_catalog",
        },
    )
    .await?;
    fixture.wait_ready().await?;
    // The production frontend transports the session's effective use_tools.
    let mut request = fixture.request.clone();
    request.use_tools = Some(Vec::new());
    let fixture = LiveOperatorFixture { request, ..fixture };
    let FixtureTools { seen, .. } = fixture.tools.as_ref();
    let seen_before = seen.lock().unwrap().len();

    let reply = run_session_bash(&fixture, "echo empty catalog").await;
    assert!(
        reply.error.is_none(),
        "pure shell must succeed with an empty catalog: {reply:?}"
    );
    assert!(reply.output.contains("empty catalog"), "{reply:?}");
    assert_eq!(
        seen.lock().unwrap().len(),
        seen_before,
        "an empty catalog must not dispatch any inner tool"
    );
    assert_eq!(
        fixture.model_calls.load(Ordering::SeqCst),
        0,
        "operator bash commands must not create inference turns"
    );
    fixture.close().await?;
    nats.kill()?;
    nats.wait()?;
    Ok(())
}
