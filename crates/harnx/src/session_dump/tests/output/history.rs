use super::*;

const LATEST_HINT: &str =
    "── Compaction 15: harnx dump session plain history --before-compaction 15 ──";
const EARLIER_HINT: &str =
    "── Compaction 5: harnx dump session plain history --before-compaction 5 ──";
const FIRST_WINDOW: &str =
    "Inspect the session log format.\nTool calls and results are separate log entries.\n";

fn hint_args(output: &str) -> DumpSessionArgs {
    let (_, command) = output.lines().next().unwrap().split_once(": ").unwrap();
    parse_dump(command.strip_suffix(" ──").unwrap())
}

fn archive(entries: &[(u64, SessionLogEntry)], selector: &str) -> String {
    let args = parse_dump(&format!(
        "harnx dump session plain history --before-compaction {}",
        shell_words::quote(selector)
    ));
    dump_with_args(entries, &args)
}

#[test]
fn printed_hints_walk_back_twice_to_the_first_message() {
    let entries = fixture();
    let current = dump(&entries);
    assert_eq!(current.lines().next(), Some(LATEST_HINT));
    let previous = dump_with_args(&entries, &hint_args(&current));
    assert_eq!(previous.lines().next(), Some(EARLIER_HINT));
    let first = dump_with_args(&entries, &hint_args(&previous));
    assert_eq!(first, FIRST_WINDOW);
}

#[test]
fn archived_output_matches_shared_fixture() {
    insta::assert_snapshot!("before_latest_compaction", archive(&fixture(), "15"));
}

#[test]
fn quoted_relative_flags_render_the_same_archived_views() {
    let entries = fixture();
    for (relative, exact) in [("1 back", "15"), ("2 back", "5")] {
        assert_eq!(archive(&entries, relative), archive(&entries, exact));
    }
}

#[test]
fn printed_hint_quotes_targets_and_preserves_remote_cluster() {
    let args = DumpSessionArgs {
        agent_name: "pkg/agent@prod".into(),
        session_id: "review's session; $(echo unsafe)".into(),
        ..dump_args()
    };
    let output = dump_with_args(&fixture(), &args);
    let hint = hint_args(&output);
    assert_eq!(
        (hint.agent_name, hint.session_id),
        (args.agent_name, args.session_id)
    );
    assert_eq!(hint.before_compaction, Some("15".parse().unwrap()));
}

#[test]
fn printed_hint_handles_option_like_names_with_separator() {
    for (agent, session) in [("plain", "-history"), ("-plain@prod", "history")] {
        let args = DumpSessionArgs {
            agent_name: agent.into(),
            session_id: session.into(),
            ..dump_args()
        };
        let output = dump_with_args(&fixture(), &args);
        let hint = hint_args(&output);
        assert_eq!(
            (hint.agent_name.as_str(), hint.session_id.as_str()),
            (agent, session)
        );
        assert_eq!(hint.before_compaction, Some("15".parse().unwrap()));
        let previous = dump_with_args(&fixture(), &hint);
        assert_eq!(
            dump_with_args(&fixture(), &hint_args(&previous)),
            FIRST_WINDOW
        );
    }
}

#[test]
fn invalid_archive_selector_emits_no_partial_transcript() {
    for selector in ["99", "7", "3 back"] {
        let args = DumpSessionArgs {
            before_compaction: Some(selector.parse().unwrap()),
            ..dump_args()
        };
        let output = capture_output(|| {
            let result = DumpRenderer::new(&args, HashMap::new(), crate::new_text_dump_sink())
                .replay(&fixture(), 0);
            assert!(result.is_err(), "{selector}");
        });
        assert_eq!(output, "");
    }
}

#[test]
fn exact_hint_stays_stable_when_another_compaction_arrives() {
    let mut entries = fixture();
    let hint = hint_args(&dump(&entries));
    let expected = dump_with_args(&entries, &hint);
    entries.push((
        24,
        SessionLogEntry::Compress {
            prompt: "Later summary.".into(),
        },
    ));
    assert_eq!(dump_with_args(&entries, &hint), expected);
}

#[test]
fn historical_view_ignores_later_edits_and_rewinds() {
    let mut entries = fixture();
    let expected = archive(&entries, "15");
    entries.extend([
        (
            24,
            SessionLogEntry::EditEntries {
                from: 7,
                to: 7,
                replacements: vec![],
            },
        ),
        (25, SessionLogEntry::Rewind { after_seq: 2 }),
    ]);
    assert_eq!(archive(&entries, "15"), expected);
}

#[test]
fn historical_replacements_with_shared_sequences_keep_replay_order() {
    let mut entries = fixture();
    let expected = archive(&entries, "15").replace(
        "Implement the shared transcript view and verify paging.\n",
        FIRST_WINDOW,
    );
    let replacements = entries[..2]
        .iter()
        .map(|(_, entry)| serde_yaml::to_string(entry).unwrap())
        .collect();
    entries[12].1 = SessionLogEntry::EditEntries {
        from: 7,
        to: 7,
        replacements,
    };
    assert_eq!(archive(&entries, "15"), expected);
}

#[test]
fn archive_at_the_first_log_entry_is_empty_without_a_hint() {
    let entries = vec![(
        1,
        SessionLogEntry::Compress {
            prompt: "Empty archive.".into(),
        },
    )];
    assert_eq!(archive(&entries, "1"), "");
}
