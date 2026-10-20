use super::*;
use crate::compaction_summary::append_recovery_note;

fn raw_prompt(entries: &[(u64, SessionLogEntry)], seq: u64) -> &str {
    let (_, SessionLogEntry::Compress { prompt }) =
        entries.iter().find(|(value, _)| *value == seq).unwrap()
    else {
        panic!("expected compaction at {seq}");
    };
    prompt
}

#[test]
fn fixture_suffixes_match_the_shared_runtime_formatter() {
    let entries = fixture();
    for (seq, from, to, count) in [(5, 1, 2, 2), (15, 8, 8, 1)] {
        let raw = raw_prompt(&entries, seq);
        let summary = display_summary(raw);
        assert_ne!(summary, raw);
        assert_eq!(
            append_recovery_note(summary.into(), (Some(from), Some(to), count)),
            raw
        );
    }
}

#[test]
fn default_marker_has_display_summary_and_leaves_raw_note_intact() {
    let entries = fixture();
    let raw = raw_prompt(&entries, 15).to_owned();
    let view = default_view(&entries).unwrap();
    assert!(!marker(&view)
        .summary
        .contains("harnx_agent_session_history_read"));
    assert_eq!(raw_prompt(&entries, 15), raw);
}

#[test]
fn archived_marker_also_has_a_display_summary() {
    let view = before_compaction(&fixture(), 15, None).unwrap();
    assert_eq!(
        marker(&view).summary,
        "The session log stores tool calls and results separately. No messages were kept."
    );
}
