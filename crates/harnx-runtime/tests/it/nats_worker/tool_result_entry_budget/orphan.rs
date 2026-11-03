//! Setup and transcript assertions for the agent-loop orphan repair test.
use super::*;
use harnx_toolset::{ToolReply, ToolRequest};
use harnx_toolset_server::invocation_journal::InvocationJournal;

async fn append_orphan_round(log: &NatsSessionLog) -> Result<u64> {
    log.append_event_async(&SessionLogEntry::ToolCalls {
        text: "working".to_string(),
        thought: None,
        calls: vec![ToolCall::new(
            "bloat_listing".to_string(),
            json!({}),
            Some("call-orphan".to_string()),
            None,
        )],
        timestamp: None,
        fence_token: None,
    })
    .await
}

fn orphan_request(key: &str, session_id: &str, round: u64) -> ToolRequest {
    ToolRequest {
        run_context: None,
        replay: None,
        call_id: "op-orphan".into(),
        operation_id: "op-orphan".into(),
        tool: "bloat_listing".into(),
        args: json!({}),
        parent_session_id: Some(key.to_string()),
        parent_agent: None,
        parent_local_session_id: Some(session_id.to_string()),
        tool_call_id: Some("call-orphan".into()),
        capabilities: Default::default(),
        tool_round: Some(round),
    }
}

pub(super) async fn seed_journaled_reply(
    js: &async_nats::jetstream::Context,
    log: &NatsSessionLog,
    key: &str,
    session_id: &str,
) -> Result<()> {
    crate::worker::append_admitted_fixture_user(log, "user-1", "continue").await?;
    // The crashed worker answered and journaled this call, but never appended
    // the ToolResults entry.
    let round = append_orphan_round(log).await?;
    let request = orphan_request(key, session_id, round);
    let reply = ToolReply {
        call_id: request.call_id.clone(),
        result: Ok(bloat_listing()),
        final_progress: None,
    };
    let journal = InvocationJournal::ensure(js, 1).await?;
    journal
        .record(
            &request,
            ("bloat_listing", "entry-budget-scope", "entry-budget"),
        )
        .await?;
    journal.complete(&request, reply).await?;
    // The JSON reply bound must preserve the original before YAML expansion.
    let rows = journal.records_for_session(key).await?;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].reply.as_ref().context("journaled reply")?.result,
        Ok(bloat_listing()),
        "the journaled reply must be the undegraded original"
    );
    Ok(())
}

pub(super) async fn run_repair(
    config: harnx_runtime::config::GlobalConfig,
    key: &str,
    metadata_store: &SessionMetadataStore,
    call_fn: harnx_runtime::agent_loop::AgentCallFn,
) -> Result<()> {
    // Enter directly: daemon recovery could hide missing metadata wiring in
    // prepare_agent_session before repair_single_orphan runs.
    let mut input = harnx_runtime::config::input::from_str(&config, "continue", None);
    input.skip_user_log_append = true;
    tokio::time::timeout(
        CI_SAFE_TIMEOUT,
        run_agent_loop_with_nats(RunAgentLoopArgs {
            cluster_key: "local",
            manage_servers: false,
            session_id: key,
            config,
            instance_id: harnx_core::instance::ServerScope::new(),
            initial_input: input,
            abort_signal: create_abort_signal(),
            token_budget: None,
            call_fn: Some(call_fn),
            lease: None,
            activation_route: harnx_runtime::SessionActivationRoute::ClusterShared,
            event_sink: None,
            after_seq_observer: None,
            session_metadata: Some(metadata_store),
            on_tool_round: None,
            working_dir: None,
        }),
    )
    .await??;
    Ok(())
}

fn assert_no_orphans(entries: &[(u64, SessionLogEntry)]) {
    // The direct loop repairs tool rounds; the daemon later settles TurnEnd.
    match reconstruct_state_from_nats(entries).turn_status {
        TurnStatus::InFlightResumable { orphans } => assert!(
            orphans.is_empty(),
            "the repaired transcript must have no orphan tool calls"
        ),
        TurnStatus::Idle => {}
        status => panic!("unexpected state after orphan repair: {status:?}"),
    }
}

pub(super) async fn assert_recovered(
    js: &async_nats::jetstream::Context,
    log: &NatsSessionLog,
    model_calls: &AtomicUsize,
) -> Result<()> {
    let entries = log.load_events_async().await?;
    assert_eq!(
        model_calls.load(Ordering::SeqCst),
        1,
        "the previously stuck session must run its pending turn"
    );
    assert_eq!(count_tool_results_with_id(&entries, "call-orphan"), 1);
    let entry = tool_results_entry(&entries)?;
    assert_entry_fits_broker(entry)?;
    let SessionLogEntry::ToolResults { results, .. } = entry else {
        unreachable!("matched ToolResults above")
    };
    assert_degraded_with_retained_original(&result_for(results, "call-orphan").output, js).await?;
    assert_no_orphans(&entries);
    Ok(())
}
