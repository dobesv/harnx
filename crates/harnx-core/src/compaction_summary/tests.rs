use super::*;

fn note() -> String {
    append_recovery_note("summary".into(), (Some(12), Some(34), 5))
}

#[test]
fn strips_the_generated_note_with_a_range() {
    assert_eq!(display_summary(&note()), "summary");
}

#[test]
fn preserves_the_legacy_format_without_a_range() {
    let expected = "summary\n\n[Earlier conversation: 2 message(s) were summarized above. \
The full pre-compaction transcript remains in this session's log; use the \
`harnx_agent_session_history_read` tool to search it by entry index, type, tool name, or text.]";
    assert_eq!(
        append_recovery_note("summary".into(), (None, None, 2)),
        expected
    );
    assert_eq!(display_summary(expected), "summary");
}

#[test]
fn incomplete_bounds_omit_the_range() {
    let expected = append_recovery_note("summary".into(), (None, None, 2));
    for bounds in [(Some(12), None, 2), (None, Some(34), 2)] {
        assert_eq!(append_recovery_note("summary".into(), bounds), expected);
    }
}

#[test]
fn strips_zero_and_maximum_counts_and_sequences() {
    for value in [0, usize::MAX] {
        let prompt = append_recovery_note("summary".into(), (Some(value), Some(value), value));
        assert_eq!(display_summary(&prompt), "summary");
    }
}

#[test]
fn preserves_summary_whitespace_and_unicode() {
    let summary = "\nRésumé: ✓\n\n";
    let prompt = append_recovery_note(summary.into(), (Some(1), Some(2), 2));
    assert_eq!(display_summary(&prompt), summary);
}

#[test]
fn an_empty_summary_remains_empty() {
    let prompt = append_recovery_note(String::new(), (None, None, 1));
    assert_eq!(display_summary(&prompt), "");
}

#[test]
fn preserves_notes_in_the_middle_and_strips_only_one_at_the_end() {
    let embedded = format!("{}\nMore summary text", note());
    assert_eq!(display_summary(&embedded), embedded);
    let doubled = append_recovery_note(note(), (None, None, 1));
    assert_eq!(display_summary(&doubled), note());
}

#[test]
fn text_after_the_note_prevents_stripping() {
    for ending in ["\n", " ", "extra text"] {
        let prompt = format!("{}{ending}", note());
        assert_eq!(display_summary(&prompt), prompt);
    }
}

#[test]
fn similar_prose_is_not_a_recovery_note() {
    let prompts = [
        "",
        "summary",
        "summary\n\n[Earlier conversation: user asked a question.]",
        "summary were summarized above.",
    ];
    for prompt in prompts {
        assert_eq!(display_summary(prompt), prompt);
    }
}

#[test]
fn every_fixed_piece_must_match_exactly() {
    for piece in [
        NOTE_START,
        COUNT_END,
        RANGE_START,
        RANGE_SEPARATOR,
        RANGE_END,
        NOTE_END,
    ] {
        let prompt = note().replacen(piece, "different", 1);
        assert_eq!(display_summary(&prompt), prompt);
    }
}

#[test]
fn count_must_be_an_unpadded_unsigned_decimal() {
    for value in ["", "five", "05", "+5", "-5", "٥", "18446744073709551616"] {
        let prompt = note().replacen("5 message(s)", &format!("{value} message(s)"), 1);
        assert_eq!(display_summary(&prompt), prompt);
    }
}

#[test]
fn range_bounds_must_be_unpadded_unsigned_decimals() {
    for bounds in [
        "12-34",
        "12–34–56",
        "–34",
        "12–",
        "012–34",
        "12–+34",
        "12–text",
    ] {
        let prompt = note().replacen("12–34", bounds, 1);
        assert_eq!(display_summary(&prompt), prompt);
    }
}
