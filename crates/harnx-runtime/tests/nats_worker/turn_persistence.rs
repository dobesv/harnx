use super::*;

async fn seed_session_and_attach_runtime(
    global_config: &Arc<RwLock<Config>>,
    jetstream: async_nats::jetstream::Context,
    session_id: &str,
) -> Result<NatsSessionLog> {
    let (metadata_store, metadata) = seed_session_metadata(&jetstream, session_id).await?;
    let backend = NatsSessionLogBackend::new(jetstream.clone(), storage_key(session_id), 1)
        .with_metadata_store(Some(metadata_store));

    let log = NatsSessionLog::new_with_replicas(jetstream, storage_key(session_id), 1);
    let mut session = metadata.base_session();
    let runtime = std::sync::Arc::new(backend.clone())
        as std::sync::Arc<dyn harnx_runtime::config::session::SessionAppendSink>;
    session.runtime = Some(std::sync::Arc::new(runtime));
    global_config.write().session = Some(session);
    Ok(log)
}

struct WorkerTurnLabels<'a> {
    cluster_key: &'a str,
    session_id: &'a str,
    prompt: &'a str,
}

struct WorkerTurnParams<'a> {
    global_config: Arc<RwLock<Config>>,
    labels: WorkerTurnLabels<'a>,
    call_fn: harnx_runtime::agent_loop::AgentCallFn,
    lease: Option<Arc<harnx_runtime::nats_lease::NatsSessionLease>>,
}

async fn run_worker_turn(params: WorkerTurnParams<'_>) -> Result<()> {
    let WorkerTurnParams {
        global_config,
        labels:
            WorkerTurnLabels {
                cluster_key,
                session_id,
                prompt,
            },
        call_fn,
        lease,
    } = params;
    let metadata_store = {
        let config = global_config.read().clone();
        let jetstream = config.nats_jetstream(cluster_key).await?;
        SessionMetadataStore::ensure(&jetstream, 1).await?
    };
    let input = harnx_runtime::config::input::from_str(&global_config, prompt, None);
    run_agent_loop_with_nats(RunAgentLoopArgs {
        cluster_key,
        manage_servers: false,
        session_id: &storage_key(session_id),
        config: global_config,
        instance_id: harnx_core::instance::ServerScope::new(),
        initial_input: input,
        abort_signal: create_abort_signal(),
        token_budget: None,
        call_fn: Some(call_fn),
        lease,
        activation_route: harnx_runtime::SessionActivationRoute::ClusterShared,
        event_sink: None,
        after_seq_observer: None,
        session_metadata: Some(&metadata_store),
        on_tool_round: None,
        working_dir: None,
    })
    .await
}

/// Stub LLM that returns: assistant with tool call on turn 1, final text on turn 2.
fn make_stub_llm_call_fn() -> harnx_runtime::agent_loop::AgentCallFn {
    let call_count = Arc::new(AtomicUsize::new(0));
    Arc::new(move |_input, _config, _abort| {
        let cc = call_count.clone();
        Box::pin(async move {
            let n = cc.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                // Turn 1: assistant calls echo tool
                Ok((
                    "let me help you".to_string(),
                    None,
                    vec![ToolCall::new(
                        "echo".to_string(),
                        json!({"message": "hello"}),
                        Some("call-echo-1".to_string()),
                        None,
                    )],
                    harnx_runtime::client::CompletionTokenUsage::default(),
                ))
            } else {
                // Turn 2: final text response
                Ok((
                    "done!".to_string(),
                    None,
                    vec![],
                    harnx_runtime::client::CompletionTokenUsage::default(),
                ))
            }
        })
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_worker_persists_full_turn_end_to_end() -> Result<()> {
    require_nextest();

    let Some(server) = spawn_nats_server().await? else {
        eprintln!("Skipping test: nats-server not available");
        return Ok(());
    };

    let global_config = Arc::new(RwLock::new(local_nats_config(NatsServerSpec {
        name: "local",
        url: server.url(),
        token: None,
    })));
    let session_id = "test-session-full-turn";

    let jetstream_ctx = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let log = seed_session_and_attach_runtime(&global_config, jetstream_ctx, session_id).await?;

    run_worker_turn(WorkerTurnParams {
        global_config: global_config.clone(),
        labels: WorkerTurnLabels {
            cluster_key: "local",
            session_id,
            prompt: "test prompt",
        },
        call_fn: make_stub_llm_call_fn(),
        lease: None,
    })
    .await?;

    // A publish ack can precede JetStream's stream metadata reflecting the
    // same sequence under load. Read the leader-authoritative tail before
    // asserting on the immediately reloaded log.
    let entries = log.load_events_latest_async().await?;
    let entries_only: Vec<SessionLogEntry> = entries.iter().map(|(_, e)| e.clone()).collect();

    // Should have: Header, User message, ToolCalls, ToolResults, final assistant Message
    assert!(
        entries.len() >= 4,
        "expected at least 4 entries, got {}",
        entries.len()
    );

    // Verify reconstruction shows Idle at end
    let state = reconstruct_state(&entries_only);
    assert_eq!(
        state.turn_status,
        TurnStatus::Idle,
        "expected Idle, got {:?}",
        state.turn_status
    );

    Ok(())
}

/// Regression test for the Aristarchus blocker: the worker execution path must
/// connect through the config-driven `nats_jetstream`/`nats_client` (applying
/// token/TLS auth), NOT a bare `async_nats::connect(url)`. Run the worker
/// against a TOKEN-AUTH nats-server with the token configured: with the old
/// bare connect this fails (auth required); with the fix it persists the turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_worker_honors_configured_token_auth() -> Result<()> {
    require_nextest();

    let token = "s3cr3t-worker-token";
    let Some(server) = common::spawn_nats_server_with_options(common::SpawnNatsServerOptions {
        auth_token: Some(token.to_string()),
    })
    .await?
    else {
        eprintln!("Skipping test: nats-server not available");
        return Ok(());
    };

    // Config carries the token so the config-driven connect can authenticate.
    let global_config = Arc::new(RwLock::new(local_nats_config(NatsServerSpec {
        name: "secure",
        url: server.url(),
        token: Some(token),
    })));
    let session_id = "test-session-token-auth";

    // Seed a header via an authenticated backend (proves auth works for setup).
    let auth_js = {
        let cfg = global_config.read().clone();
        cfg.nats_jetstream("secure").await?
    };
    let log = seed_session_and_attach_runtime(&global_config, auth_js, session_id).await?;

    // The worker connects internally via the config-driven path; this only
    // succeeds if it applies the configured token.
    run_worker_turn(WorkerTurnParams {
        global_config: global_config.clone(),
        labels: WorkerTurnLabels {
            cluster_key: "secure",
            session_id,
            prompt: "test prompt",
        },
        call_fn: make_stub_llm_call_fn(),
        lease: None,
    })
    .await?;

    let entries = log.load_events_latest_async().await?;
    assert!(
        entries.len() >= 4,
        "expected the authenticated worker to persist a full turn, got {} entries",
        entries.len()
    );
    Ok(())
}
