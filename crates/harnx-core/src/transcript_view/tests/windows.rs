use super::*;

#[test]
fn empty_log_is_an_empty_plain_transcript() {
    let view = default_view(&[]).unwrap();
    assert!(view.compaction.is_none());
    assert!(view.entries.is_empty());
}

#[test]
fn no_compaction_preserves_plain_messages() {
    let view = default_view(&through(3)).unwrap();
    assert_eq!(view.entries, through(2));
    assert!(view.compaction.is_none());
}

#[test]
fn one_compaction_precedes_later_messages() {
    let view = default_view(&through(12)).unwrap();
    assert_eq!(displayed_seqs(&view), [5, 7, 8, 9, 10, 11, 12]);
    assert_eq!(marker(&view).earlier_compaction, None);
}

#[test]
fn zero_kept_messages_still_have_a_summary() {
    let view = default_view(&through(6)).unwrap();
    assert_eq!(displayed_seqs(&view), [5]);
    assert_eq!(marker(&view).kept_count, Some(0));
    assert!(!marker(&view).summary.is_empty());
}

#[test]
fn two_compactions_walk_back_to_the_first_message() {
    let entries = fixture();
    let current = default_view(&entries).unwrap();
    let previous = before_compaction(&entries, marker(&current).compaction_seq, None).unwrap();
    let first = before_compaction(&entries, marker(&previous).compaction_seq, None).unwrap();
    assert_eq!(displayed_seqs(&current), [15, 16, 17, 18, 19, 21, 22]);
    assert_eq!(displayed_seqs(&previous), [5, 7, 8, 9, 10, 11, 12]);
    assert_eq!(displayed_seqs(&first), [1, 2]);
}

#[test]
fn marker_describes_the_whole_archived_window_not_the_dropped_complement() {
    let view = default_view(&fixture()).unwrap();
    let expected = CompactionMarker {
        compaction_seq: 15,
        summary: "Earlier log inspection is archived. The latest prompt, verification round and final reply remain verbatim.".into(),
        from_seq: 6,
        to_seq: 14,
        kept_count: None,
        earlier_compaction: Some(5),
    };
    assert_eq!(marker(&view), &expected);
}

#[test]
fn first_marker_has_no_earlier_compaction() {
    let view = before_compaction(&fixture(), 15, None).unwrap();
    assert_eq!((marker(&view).from_seq, marker(&view).to_seq), (1, 4));
    assert_eq!(marker(&view).earlier_compaction, None);
}

#[test]
fn c2_prompt_and_tail_are_copies_of_non_contiguous_originals() {
    let entries = fixture();
    for (original, copy) in [(7, 16), (10, 17), (11, 18), (12, 19)] {
        assert_eq!(entries[original - 1].1, entries[copy - 1].1);
    }
    let archived = before_compaction(&entries, 15, None).unwrap();
    assert_eq!(displayed_seqs(&archived), [5, 7, 8, 9, 10, 11, 12]);
}

#[test]
fn default_contains_only_relogged_copies_and_later_activity() {
    let view = default_view(&fixture()).unwrap();
    assert_eq!(seqs(&view), [16, 17, 18, 19, 20, 21, 22]);
}

#[test]
fn identical_later_content_is_not_deduplicated() {
    let entries = [
        message(1, "same"),
        compress(2),
        message(3, "same"),
        message(4, "same"),
    ];
    let view = default_view(&entries).unwrap();
    assert_eq!(seqs(&view), [3, 4]);
    assert_eq!(marker(&view).kept_count, None);
}

#[test]
fn shared_json_fixture_and_views_round_trip() {
    let entries = fixture();
    let log_json = serde_json::to_string(&entries).unwrap();
    assert_eq!(
        serde_json::from_str::<Vec<(u64, SessionLogEntry)>>(&log_json).unwrap(),
        entries
    );
    let view = default_view(&entries).unwrap();
    let view_json = serde_json::to_string(&view).unwrap();
    assert_eq!(
        serde_json::from_str::<TranscriptView>(&view_json).unwrap(),
        view
    );
}

#[test]
fn consecutive_compactions_have_an_empty_archived_range() {
    let entries = [compress(1), compress(2)];
    let view = default_view(&entries).unwrap();
    assert_eq!((marker(&view).from_seq, marker(&view).to_seq), (2, 1));
    assert_eq!(
        displayed_seqs(&before_compaction(&entries, 2, None).unwrap()),
        [1]
    );
}

#[test]
fn before_compaction_at_first_log_entry_is_an_empty_plain_view() {
    let entries = [compress(1), message(2, "later")];
    let view = before_compaction(&entries, 1, None).unwrap();
    assert!(view.compaction.is_none());
    assert!(view.entries.is_empty());
}
