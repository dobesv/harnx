use super::*;

fn fixture() -> Vec<(u64, SessionLogEntry)> {
    serde_json::from_str(include_str!(
        "../../../harnx-core/tests/fixtures/compaction-history.json"
    ))
    .unwrap()
}

fn render(entries: &[(u64, SessionLogEntry)]) -> String {
    render_transcript_text(entries).unwrap()
}

#[test]
fn overlay_shows_shared_fixture_marker_then_kept_once_then_later() {
    insta::assert_snapshot!("compacted_overlay", render(&fixture()));
}

#[test]
fn overlay_zero_kept_still_shows_summary() {
    assert_eq!(
        render(&fixture()[..6]),
        "── Compaction 5: earlier history archived ──\nThe session log stores tool calls and results separately. No messages were kept."
    );
}

#[test]
fn overlay_uncompacted_output_is_unchanged() {
    assert_eq!(
        render(&fixture()[..4]),
        "── user ──\nInspect the session log format.\n\n── assistant ──\nTool calls and results are separate log entries."
    );
}

#[test]
fn overlay_resolves_edits_with_shared_out_of_order_sequences() {
    let mut entries = fixture();
    let expected = render(&entries).replace(
        "── user ──\nImplement the shared transcript view and verify paging.",
        &render(&entries[20..22]),
    );
    let replacements = entries[20..22]
        .iter()
        .map(|(_, entry)| serde_yaml::to_string(entry).unwrap())
        .collect();
    entries.push((
        24,
        SessionLogEntry::EditEntries {
            from: 16,
            to: 16,
            replacements,
        },
    ));
    assert_eq!(render(&entries), expected);
}

#[test]
fn overlay_resolves_rewind_before_selecting_marker() {
    let mut entries = fixture();
    entries.push((24, SessionLogEntry::Rewind { after_seq: 6 }));
    assert_eq!(render(&entries), render(&fixture()[..6]));
}

#[test]
fn overlay_propagates_invalid_log_errors() {
    let error = render_transcript_text(&[(0, SessionLogEntry::Clear)]).unwrap_err();
    assert_eq!(
        error.to_string(),
        "session log sequences must be nonzero and strictly increasing"
    );
}
