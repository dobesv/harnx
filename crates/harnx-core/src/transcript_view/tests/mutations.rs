use super::*;

fn delete(seq: u64, target: usize) -> (u64, SessionLogEntry) {
    (
        seq,
        SessionLogEntry::EditEntries {
            from: target,
            to: target,
            replacements: vec![],
        },
    )
}

#[test]
fn default_resolves_edits_before_selecting_the_window() {
    let mut entries = fixture();
    entries.push(delete(24, 21));
    assert_eq!(
        seqs(&default_view(&entries).unwrap()),
        [16, 17, 18, 19, 20, 22]
    );
}

#[test]
fn later_edits_do_not_change_a_historical_view() {
    let mut entries = fixture();
    let before = before_compaction(&entries, 15, None).unwrap();
    entries.push(delete(24, 7));
    assert_eq!(before_compaction(&entries, 15, None).unwrap(), before);
}

#[test]
fn marker_archive_link_is_frozen_at_its_compaction() {
    let mut entries = fixture();
    entries.push(delete(24, 5));
    let current = default_view(&entries).unwrap();
    assert_eq!(marker(&current).earlier_compaction, Some(5));
    let previous = before_compaction(&entries, 15, None).unwrap();
    assert_eq!(marker(&previous).compaction_seq, 5);
}

#[test]
fn rewind_restores_the_previous_default_window() {
    let mut entries = fixture();
    entries.push((24, SessionLogEntry::Rewind { after_seq: 14 }));
    let view = default_view(&entries).unwrap();
    assert_eq!(displayed_seqs(&view), [5, 7, 8, 9, 10, 11, 12]);
    assert_eq!(before_compaction(&entries, 15, None).unwrap(), view);
}

#[test]
fn historical_view_resolves_only_mutations_before_its_boundary() {
    let entries = [
        message(1, "deleted"),
        message(2, "kept"),
        delete(3, 1),
        compress(4),
    ];
    let view = before_compaction(&entries, 4, None).unwrap();
    assert_eq!(view.entries, [message(2, "kept")]);
}

#[test]
fn replacement_rows_share_their_mutation_sequence_and_keep_replay_order() {
    let replacements = [message(1, "replacement one"), message(1, "replacement two")]
        .iter()
        .map(|(_, entry)| serde_yaml::to_string(entry).unwrap())
        .collect();
    let edit = (
        3,
        SessionLogEntry::EditEntries {
            from: 1,
            to: 1,
            replacements,
        },
    );
    let entries = [message(1, "old"), message(2, "second"), edit, compress(4)];
    let whole = before_compaction(&entries, 4, None).unwrap();
    let page = before_compaction(&entries, 4, Some(3..=3)).unwrap();
    assert_eq!(seqs(&whole), [3, 3, 2]);
    assert_eq!(seqs(&page), [3, 3]);
}

#[test]
fn read_only_views_do_not_modify_input_entries() {
    let entries = fixture();
    let original = entries.clone();
    default_view(&entries).unwrap();
    before_compaction(&entries, 15, Some(7..=12)).unwrap();
    assert_eq!(entries, original);
}
