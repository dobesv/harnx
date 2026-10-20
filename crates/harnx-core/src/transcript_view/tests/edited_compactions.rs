use super::*;

fn replace(seq: u64, target: usize, entries: Vec<SessionLogEntry>) -> (u64, SessionLogEntry) {
    let replacements = entries
        .iter()
        .map(|entry| serde_yaml::to_string(entry).unwrap())
        .collect();
    (
        seq,
        SessionLogEntry::EditEntries {
            from: target,
            to: target,
            replacements,
        },
    )
}

fn replacement_window() -> Vec<(u64, SessionLogEntry)> {
    vec![
        message(1, "original"),
        message(2, "later"),
        replace(3, 1, vec![compress(0).1, message(0, "replacement tail").1]),
    ]
}

#[test]
fn edited_compress_keeps_boundary_slicing_without_an_unaddressable_marker() {
    let view = default_view(&replacement_window()).unwrap();
    assert!(view.compaction.is_none());
    assert_eq!(
        view.entries,
        [message(3, "replacement tail"), message(2, "later")]
    );
}

#[test]
fn multiple_replacement_compactions_at_one_sequence_use_the_last_boundary() {
    let replacements = vec![compress(0).1, message(0, "middle").1, compress(0).1];
    let entries = [
        message(1, "original"),
        message(2, "later"),
        replace(3, 1, replacements),
    ];
    let view = default_view(&entries).unwrap();
    assert!(view.compaction.is_none());
    assert_eq!(view.entries, [message(2, "later")]);
}

#[test]
fn replacing_a_raw_compaction_does_not_reuse_its_original_marker() {
    let entries = [
        message(1, "original"),
        compress(2),
        replace(3, 2, vec![compress(0).1]),
    ];
    let view = default_view(&entries).unwrap();
    assert!(view.compaction.is_none());
    assert!(view.entries.is_empty());
}

#[test]
fn edited_compress_sequence_is_not_a_browse_back_target() {
    let error = before_compaction(&replacement_window(), 3, None).unwrap_err();
    assert_eq!(error.to_string(), "sequence 3 is not a compaction");
}

#[test]
fn raw_compaction_after_edited_boundary_does_not_link_to_the_mutation() {
    let mut entries = replacement_window();
    entries.push(compress(4));
    let current = default_view(&entries).unwrap();
    assert_eq!(marker(&current).earlier_compaction, None);
    let previous = before_compaction(&entries, 4, None).unwrap();
    assert!(previous.compaction.is_none());
    assert_eq!(seqs(&previous), [3, 2]);
}

#[test]
fn archive_bounds_after_edited_boundary_cover_all_remaining_physical_sequences() {
    let mut entries = replacement_window();
    entries.push(compress(4));
    let current = default_view(&entries).unwrap();
    let range = marker(&current).archive_range();
    assert_eq!(range, Some(2..=3));
    let page = before_compaction(&entries, 4, range).unwrap();
    assert_eq!(seqs(&page), [3, 2]);
}

#[test]
fn empty_window_after_edited_boundary_has_no_archive_range() {
    let entries = [
        message(1, "original"),
        replace(2, 1, vec![compress(0).1]),
        compress(3),
    ];
    let current = default_view(&entries).unwrap();
    assert_eq!(marker(&current).archive_range(), None);
    assert_eq!(marker(&current).earlier_compaction, None);
}

#[test]
fn a_later_edit_does_not_change_an_earlier_raw_compactions_archive() {
    let entries = [
        message(1, "original"),
        compress(2),
        replace(3, 1, vec![compress(0).1]),
    ];
    let current = default_view(&entries).unwrap();
    assert_eq!(marker(&current).compaction_seq, 2);
    let previous = before_compaction(&entries, 2, None).unwrap();
    assert_eq!(previous.entries, [message(1, "original")]);
}
