use super::*;

const EMPTY_MARKER: &str = "agent:── Compaction 5: earlier history archived ──\nThe session log stores tool calls and results separately. No messages were kept.\n";
const DEFAULT_UPDATES: &[&str] = &[
    "agent:── Compaction 15: earlier history archived ──\nEarlier log inspection is archived. The latest prompt, verification round and final reply remain verbatim.\n",
    "user:Implement the shared transcript view and verify paging.",
    "tool-start:retained-round:exec",
    "agent:Check the transcript view tests.",
    "tool-end:retained-round:{\"exit_code\":0,\"stdout\":\"Transcript view tests passed.\"}",
    "agent:The shared view is implemented. Paging tests passed.",
    "user:Show the previous compaction's history.",
    "agent:The earlier window contains the original prompt and both tool rounds.",
];

fn shared_entries() -> Vec<SessionLogEntry> {
    let entries: Vec<(u64, SessionLogEntry)> = serde_json::from_str(include_str!(
        "../../../../../harnx-core/tests/fixtures/compaction-history.json"
    ))
    .unwrap();
    entries.into_iter().map(|(_, entry)| entry).collect()
}

async fn assert_loaded_updates(entries: Vec<SessionLogEntry>, expected: &[&str]) -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let config = test_config(&server.url);
    create_fixture(
        &config,
        FixtureSpec {
            agent: AGENT_NAME,
            session_id: "compacted",
            entries,
        },
    )
    .await?;
    let agent = new_agent(&config);
    let mut client = attach_test_client(Arc::clone(&agent), PermissionReply::Reject).await?;
    let updates = load_updates(&mut client, "compacted", expected.len()).await?;
    let actual: Vec<_> = updates.into_iter().map(replay_summary).collect();
    assert_eq!(actual, expected);
    assert!(agent.session_last_touched("compacted").await.is_some());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_load_shared_fixture_sends_marker_kept_once_then_later() -> Result<()> {
    assert_loaded_updates(shared_entries(), DEFAULT_UPDATES).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_load_zero_kept_sends_summary_as_agent_text() -> Result<()> {
    let mut entries = shared_entries();
    entries.truncate(6);
    assert_loaded_updates(entries, &[EMPTY_MARKER]).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_load_uncompacted_fixture_is_unchanged() -> Result<()> {
    let mut entries = shared_entries();
    entries.truncate(4);
    assert_loaded_updates(
        entries,
        &[
            "user:Inspect the session log format.",
            "agent:Tool calls and results are separate log entries.",
        ],
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_load_keeps_raw_log_for_shared_sequence_replacements() -> Result<()> {
    let mut entries = shared_entries();
    let replacements = entries[20..22]
        .iter()
        .map(|entry| harnx_runtime::config::yaml_doc(entry).unwrap())
        .collect();
    entries.push(SessionLogEntry::EditEntries {
        from: 16,
        to: 16,
        replacements,
    });
    let mut expected = DEFAULT_UPDATES.to_vec();
    expected.splice(1..2, DEFAULT_UPDATES[6..8].iter().copied());
    assert_loaded_updates(entries, &expected).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_load_resolves_rewind_before_selecting_marker() -> Result<()> {
    let mut entries = shared_entries();
    entries.push(SessionLogEntry::Rewind { after_seq: 6 });
    assert_loaded_updates(entries, &[EMPTY_MARKER]).await
}

#[test]
fn info_notice_is_visible_as_agent_text_only_during_replay() {
    use harnx_acp_server::event_map::{
        agent_event_to_replay_update, agent_event_to_session_update,
    };
    use harnx_core::event::NoticeEvent;

    let event = AgentEvent::Notice(NoticeEvent::Info(
        "earlier history archived\nsummary".into(),
    ));
    let replay = agent_event_to_replay_update(event.clone(), CLUSTER).unwrap();
    assert_eq!(
        replay_summary(replay),
        "agent:earlier history archived\nsummary"
    );
    assert!(agent_event_to_session_update(event).is_none());
}
