//! Shared-log parity through runtime session replay and the TUI transcript builder.

use std::collections::HashMap;

use harnx_core::session::{Session, SessionLogEntry};
use harnx_core::transcript_view;
use harnx_runtime::config::session::replay_nats_entries_into_session;

use crate::lifecycle::{build_transcript_with_compaction, messages_to_transcript_items};
use crate::types::TranscriptItem;

pub(super) fn fixture() -> Vec<(u64, SessionLogEntry)> {
    serde_json::from_str(include_str!(
        "../../../harnx-core/tests/fixtures/compaction-history.json"
    ))
    .expect("shared compaction fixture")
}

pub(super) fn replay(entries: &[(u64, SessionLogEntry)]) -> Session {
    let entries = entries
        .iter()
        .map(|(seq, entry)| (usize::try_from(*seq).unwrap(), entry.clone()))
        .collect::<Vec<_>>();
    replay_nats_entries_into_session(&entries, "d2-fixture", Session::default(), false)
        .expect("normal NATS session replay")
}

fn transcript(entries: &[(u64, SessionLogEntry)]) -> Vec<TranscriptItem> {
    let session = replay(entries);
    build_transcript_with_compaction(
        &session.compressed_messages,
        &session.messages,
        session.compaction_summary.as_deref(),
        &HashMap::new(),
    )
}

// Ignore render caches and wall-clock tool anchors, but retain every displayed row.
pub(super) fn display_rows(items: &[TranscriptItem]) -> Vec<String> {
    items.iter().map(display_row).collect()
}

fn display_row(item: &TranscriptItem) -> String {
    match item {
        TranscriptItem::CompactionMarker { summary_text, .. } => {
            format!("marker: {summary_text}")
        }
        TranscriptItem::UserText { text, seq, .. } => format!("user {seq:?}: {text}"),
        TranscriptItem::AssistantText { text, seq, .. } => format!("assistant {seq:?}: {text}"),
        TranscriptItem::ToolCall {
            tool_name,
            body,
            seq,
            ..
        } => {
            format!("tool {seq:?}: {tool_name}\n{body:?}")
        }
        TranscriptItem::ToolResultMarkdown { text, .. } => format!("result: {text}"),
        other => panic!("unexpected fixture transcript row: {other:?}"),
    }
}

fn marker(items: &[TranscriptItem]) -> (&str, &str) {
    let markers = items
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::CompactionMarker {
                summary_text,
                detail_text,
                ..
            } => Some((summary_text.as_str(), detail_text.as_str())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(markers.len(), 1, "only the latest compaction is inline");
    markers[0]
}

fn assert_fragments(text: &str, fragments: &[&str]) {
    for fragment in fragments {
        assert!(text.contains(fragment), "missing {fragment:?} in {text:?}");
    }
}

#[test]
fn d2_fixture_shows_latest_marker_kept_then_later() {
    let entries = fixture();
    let view = transcript_view::default_view(&entries).unwrap();
    let latest = view.compaction.unwrap();
    let items = transcript(&entries);
    assert_eq!(latest.compaction_seq, 15);
    assert_eq!(marker(&items).0, latest.summary);
    assert_default_rows(&items, &view.entries);
}

fn assert_default_rows(items: &[TranscriptItem], entries: &[(u64, SessionLogEntry)]) {
    // D1 selects the log window; runtime pairs 17/18 into one tool message.
    // Compare every TUI row, including tool prose, call arguments and result body.
    let active = replay(entries);
    let expected = messages_to_transcript_items(&active.messages, &HashMap::new());
    assert!(matches!(
        items.first(),
        Some(TranscriptItem::CompactionMarker { .. })
    ));
    assert_eq!(display_rows(&items[1..]), display_rows(&expected));
    insta::assert_snapshot!("d2_compacted_transcript", display_rows(items).join("\n"));
}

#[test]
fn d2_fixture_archived_sequences_are_not_inline() {
    let items = transcript(&fixture());
    let sequences = items
        .iter()
        .filter_map(TranscriptItem::seq)
        .collect::<Vec<_>>();
    // Tool prose and call both reference 17; its result row has no separate seq.
    assert_eq!(sequences, [16, 17, 17, 19, 21, 22]);
    let rows = display_rows(&items).join("\n");
    for archived_only in [
        "Inspect the session log format.",
        "Inspect the archived middle of the turn.",
    ] {
        assert!(!rows.contains(archived_only), "archived row leaked inline");
    }
}

#[test]
fn d2_fixture_marker_detail_contains_archived_messages() {
    let items = transcript(&fixture());
    let (summary, detail) = marker(&items);
    assert_fragments(
        detail,
        &[
            "Inspect the session log format.",
            "Tool calls and results are separate log entries.",
            "Implement the shared transcript view and verify paging.",
            "Inspect the archived middle of the turn.",
            "path: crates/harnx-core/src/session.rs",
            "Compress archives the previous window; kept messages are re-logged.",
            "Check the transcript view tests.",
            "command: cargo nextest run -p harnx-core transcript_view",
            "stdout: Transcript view tests passed.",
            "The shared view is implemented. Paging tests passed.",
        ],
    );
    assert!(!summary.contains("were summarized above"));
    assert!(!detail.contains("Show the previous compaction's history."));
}

fn assert_summary_only_transcript(
    entries: &[(u64, SessionLogEntry)],
    expected_seq: Option<u64>,
    expected_fragments: &[&str],
) {
    let items = transcript(entries);
    let latest = transcript_view::default_view(entries)
        .unwrap()
        .compaction
        .unwrap();
    if let Some(seq) = expected_seq {
        assert_eq!(latest.compaction_seq, seq);
    }
    assert_eq!(
        display_rows(&items),
        [format!("marker: {}", latest.summary)]
    );
    assert_fragments(marker(&items).1, expected_fragments);
}

#[test]
fn d2_fixture_zero_kept_shows_summary_instead_of_blank_transcript() {
    let entries = fixture()
        .into_iter()
        .take_while(|(seq, _)| *seq <= 6)
        .collect::<Vec<_>>();
    assert_summary_only_transcript(&entries, Some(5), &["Inspect the session log format."]);
}

#[test]
fn d2_fixture_uncompacted_session_has_no_marker() {
    let entries = fixture().into_iter().take(4).collect::<Vec<_>>();
    let items = transcript(&entries);
    let seqs = items
        .iter()
        .filter_map(TranscriptItem::seq)
        .collect::<Vec<_>>();
    assert_eq!(seqs, [1, 2]);
    assert_eq!(items.len(), 2);
}

#[test]
fn d2_fixture_summary_survives_an_empty_archive() {
    let entries = fixture()
        .into_iter()
        .filter(|(seq, _)| (5..=6).contains(seq))
        .collect::<Vec<_>>();
    assert_summary_only_transcript(&entries, None, &["archived messages: 0"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn d2_fixture_enter_opens_archived_transcript_nats() {
    let temp = tempfile::TempDir::new().unwrap();
    let _lock = super::ENV_LOCK.lock().await;
    let _environment = super::TestEnvironment::set(temp.path());
    let config = super::d4_fixture::fixture_config(&fixture()).await;
    let mut harness = super::TuiTestHarness::with_config(config).await;
    super::d4_fixture::select_marker(harness.tui()).await;
    super::d4_fixture::press(harness.tui(), crossterm::event::KeyCode::Enter).await;
    let view = super::d4_fixture::archive(harness.tui());
    assert_eq!(view.before, 15);
    assert_eq!(view.marker.as_ref().unwrap().compaction_seq, 5);
    assert!(view
        .items
        .iter()
        .any(|item| matches!(item, TranscriptItem::ToolCall { .. })));
}
