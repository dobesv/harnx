use super::*;

#[test]
fn inclusive_pages_concatenate_without_duplicates() {
    let entries = fixture();
    let first = before_compaction(&entries, 15, Some(5..=9)).unwrap();
    let last = before_compaction(&entries, 15, Some(10..=14)).unwrap();
    let whole = before_compaction(&entries, 15, None).unwrap();
    assert_eq!([seqs(&first), seqs(&last)].concat(), seqs(&whole));
    assert_eq!(first.compaction, whole.compaction);
    assert!(last.compaction.is_none());
}

#[test]
fn marker_is_included_only_if_the_page_reaches_it() {
    let entries = fixture();
    let marker_only = before_compaction(&entries, 15, Some(5..=5)).unwrap();
    let after = before_compaction(&entries, 15, Some(6..=6)).unwrap();
    assert_eq!(displayed_seqs(&marker_only), [5]);
    assert_eq!(marker(&marker_only).to_seq, 4);
    assert!(after.compaction.is_none());
}

#[test]
fn paging_includes_both_endpoints() {
    let view = before_compaction(&fixture(), 15, Some(7..=12)).unwrap();
    assert_eq!(seqs(&view), [7, 8, 9, 10, 11, 12]);
}

#[test]
fn paging_can_address_one_tool_result_entry() {
    let view = before_compaction(&fixture(), 15, Some(9..=9)).unwrap();
    assert_eq!(seqs(&view), [9]);
    assert!(matches!(
        view.entries[0].1,
        SessionLogEntry::ToolResults { .. }
    ));
}

#[test]
fn disjoint_ranges_return_empty_pages() {
    let entries = fixture();
    for range in [0..=4, 15..=u64::MAX] {
        let page = before_compaction(&entries, 15, Some(range)).unwrap();
        assert!(displayed_seqs(&page).is_empty());
        assert!(page.entries.is_empty());
    }
}

#[test]
fn a_sequence_gap_is_an_empty_page() {
    let view = before_compaction(&fixture(), 15, Some(13..=13)).unwrap();
    assert!(view.entries.is_empty());
}

#[test]
fn physical_gaps_are_not_renumbered() {
    let entries = [message(3, "first"), message(9, "second"), compress(17)];
    let view = before_compaction(&entries, 17, Some(4..=9)).unwrap();
    assert_eq!(seqs(&view), [9]);
    assert_eq!(marker(&default_view(&entries).unwrap()).from_seq, 3);
}

#[test]
fn reversed_range_is_an_error() {
    let range = RangeInclusive::new(9, 7);
    let error = before_compaction(&fixture(), 15, Some(range)).unwrap_err();
    assert_eq!(error.to_string(), "transcript sequence range is reversed");
}

#[test]
fn unknown_and_non_compaction_sequences_are_errors() {
    for (seq, expected) in [
        (99, "unknown compaction sequence 99"),
        (7, "sequence 7 is not a compaction"),
    ] {
        let error = before_compaction(&fixture(), seq, None).unwrap_err();
        assert_eq!(error.to_string(), expected);
    }
}

#[test]
fn raw_sequences_must_be_nonzero_and_strictly_increasing() {
    for numbers in [[0, 1], [2, 1], [1, 1]] {
        let entries = numbers.map(|seq| message(seq, "message"));
        let error = default_view(&entries).unwrap_err();
        assert_eq!(
            error.to_string(),
            "session log sequences must be nonzero and strictly increasing"
        );
    }
}

#[test]
fn archive_range_returns_inclusive_bounds() {
    let view = default_view(&fixture()).unwrap();
    assert_eq!(marker(&view).archive_range(), Some(6..=14));
}

#[test]
fn archive_range_is_none_for_consecutive_compactions() {
    let view = default_view(&[compress(1), compress(2)]).unwrap();
    assert_eq!(marker(&view).archive_range(), None);
}

#[test]
fn archive_range_is_none_for_a_compaction_at_the_first_entry() {
    let view = default_view(&[compress(1)]).unwrap();
    assert_eq!(marker(&view).archive_range(), None);
}

#[test]
fn archive_range_includes_a_single_entry() {
    let view = default_view(&[message(1, "archived"), compress(2)]).unwrap();
    assert_eq!(marker(&view).archive_range(), Some(1..=1));
}
