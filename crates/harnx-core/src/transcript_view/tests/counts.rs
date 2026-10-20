use super::*;
use crate::session::CompactOutcome;

#[test]
fn end_of_log_does_not_prove_compaction_finished() {
    let view = default_view(&through(5)).unwrap();
    assert_eq!(marker(&view).kept_count, None);
}

#[test]
fn immediate_turn_end_proves_zero_kept_messages() {
    let mut entries = through(5);
    entries.push((
        6,
        SessionLogEntry::TurnEnd {
            through_seq: 1,
            fence_token: 1,
            timestamp: None,
            usage: None,
        },
    ));
    let view = default_view(&entries).unwrap();
    assert_eq!(marker(&view).kept_count, Some(0));
    assert!(view.entries.is_empty());
}

#[test]
fn failed_receipt_does_not_prove_a_count() {
    let mut entries = through(5);
    entries.push((
        6,
        SessionLogEntry::compact_result("failed", CompactOutcome::Failed("interrupted".into())),
    ));
    assert_eq!(marker(&default_view(&entries).unwrap()).kept_count, None);
}

#[test]
fn nonempty_completed_span_has_no_guessed_count() {
    let view = default_view(&through(20)).unwrap();
    assert_eq!(marker(&view).kept_count, None);
}

#[test]
fn interleaved_admission_is_not_counted_as_retention() {
    let mut entries = through(5);
    entries.push(message(6, "queued while compaction was writing"));
    entries.push((
        7,
        SessionLogEntry::compact_result("keep-nothing", CompactOutcome::Compacted),
    ));
    let view = default_view(&entries).unwrap();
    assert_eq!(marker(&view).kept_count, None);
    assert_eq!(displayed_seqs(&view), [5, 6]);
}

#[test]
fn a_gap_before_the_receipt_does_not_prove_zero_kept() {
    let mut entries = through(5);
    entries.push((
        9,
        SessionLogEntry::compact_result("keep-nothing", CompactOutcome::Compacted),
    ));
    assert_eq!(marker(&default_view(&entries).unwrap()).kept_count, None);
}
