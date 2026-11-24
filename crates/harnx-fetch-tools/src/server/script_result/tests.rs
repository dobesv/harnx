use super::*;

#[test]
fn whole_lines_fit_under_the_cap() {
    let mut notes = Vec::new();
    let (text, cut) = keep_whole_lines("aaaa\nbbbb\ncccc\n", 12, &mut notes);
    assert_eq!((text.as_str(), cut), ("aaaa\nbbbb\n", true));
    assert_eq!(
        notes,
        ["cut at 10 of 15 bytes. Use max_output_bytes to see more"]
    );
}

#[test]
fn a_first_line_over_the_cap_is_cut_on_a_character_boundary() {
    let mut notes = Vec::new();
    let (text, cut) = keep_whole_lines("日本語\n", 4, &mut notes);
    assert_eq!((text.as_str(), cut), ("日", true));
}

#[test]
fn text_within_the_cap_has_no_note() {
    let mut notes = Vec::new();
    assert_eq!(
        keep_whole_lines("abc", 3, &mut notes),
        ("abc".to_owned(), false)
    );
    assert!(notes.is_empty());
}
