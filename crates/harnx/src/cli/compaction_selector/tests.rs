use super::*;

fn fixture() -> Vec<(u64, SessionLogEntry)> {
    serde_json::from_str(include_str!(
        "../../../../harnx-core/tests/fixtures/compaction-history.json"
    ))
    .unwrap()
}

fn select(entries: &[(u64, SessionLogEntry)], value: &str) -> Result<TranscriptView> {
    value.parse::<CompactionSelector>()?.view(entries)
}

#[test]
fn parses_positive_sequence_and_relative_count() {
    assert_eq!(
        "15".parse::<CompactionSelector>().unwrap(),
        CompactionSelector::Sequence(NonZeroU64::new(15).unwrap())
    );
    assert_eq!(
        "2 back".parse::<CompactionSelector>().unwrap(),
        CompactionSelector::Back(NonZeroU64::new(2).unwrap())
    );
}

#[test]
fn rejects_zero_invalid_and_overflowed_selectors() {
    for value in [
        "",
        "0",
        "0 back",
        "-1",
        "-1 back",
        "1.5",
        "back",
        "1 backs",
        "1 back extra",
        "18446744073709551616",
        "18446744073709551616 back",
    ] {
        let error = value.parse::<CompactionSelector>().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("compaction sequence or quoted \"N back\""),
            "{value}: {error}"
        );
    }
}

#[test]
fn relative_counts_match_exact_fixture_windows() {
    let entries = fixture();
    for (relative, exact) in [("1 back", "15"), ("2 back", "5")] {
        assert_eq!(
            select(&entries, relative).unwrap(),
            select(&entries, exact).unwrap()
        );
    }
}

#[test]
fn rejects_unknown_and_non_compaction_sequences() {
    let entries = fixture();
    for (value, expected) in [
        ("99", "unknown compaction sequence 99"),
        ("7", "sequence 7 is not a compaction"),
    ] {
        assert_eq!(select(&entries, value).unwrap_err().to_string(), expected);
    }
}

#[test]
fn rejects_too_many_back_with_available_count() {
    let error = select(&fixture(), "3 back").unwrap_err();
    assert_eq!(
        error.to_string(),
        "cannot go 3 compactions back: only 2 available from the current view"
    );
}

#[test]
fn rejects_relative_selector_without_a_compaction() {
    let entries = fixture();
    for prefix in [&entries[..0], &entries[..4]] {
        assert_eq!(
            select(prefix, "1 back").unwrap_err().to_string(),
            "cannot go 1 compactions back: only 0 available from the current view"
        );
    }
}

#[test]
fn relative_counts_follow_ancestry_not_rewound_raw_markers() {
    let mut entries = fixture();
    entries.extend([
        (24, SessionLogEntry::Rewind { after_seq: 6 }),
        (
            25,
            SessionLogEntry::Compress {
                prompt: "A new branch.".into(),
            },
        ),
    ]);
    assert_eq!(
        select(&entries, "1 back").unwrap(),
        select(&entries, "25").unwrap()
    );
    assert_eq!(
        select(&entries, "2 back").unwrap(),
        select(&entries, "5").unwrap()
    );
    assert!(select(&entries, "3 back").is_err());
}

#[test]
fn exact_sequence_still_opens_a_rewound_archive() {
    let mut entries = fixture();
    let archive = select(&entries, "15").unwrap();
    entries.push((24, SessionLogEntry::Rewind { after_seq: 2 }));
    assert_eq!(select(&entries, "15").unwrap(), archive);
    assert!(select(&entries, "1 back").is_err());
}
