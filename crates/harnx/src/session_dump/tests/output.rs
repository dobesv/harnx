use super::*;
use crate::test_utils::capture_output;
use harnx_runtime::config::SessionFormat;
use serde::Deserialize;

fn dump_format(entries: &[(u64, SessionLogEntry)], format: SessionFormat) -> String {
    let args = DumpSessionArgs {
        format,
        ..dump_args()
    };
    dump_with_args(entries, &args)
}

fn dump_with_args(entries: &[(u64, SessionLogEntry)], args: &DumpSessionArgs) -> String {
    capture_output(|| {
        DumpRenderer::new(args, HashMap::new(), crate::new_text_dump_sink())
            .replay(entries, 0)
            .unwrap();
    })
}

fn dump(entries: &[(u64, SessionLogEntry)]) -> String {
    dump_format(entries, SessionFormat::Text)
}

fn dump_after(entries: &[(u64, SessionLogEntry)], after_seq: u64) -> String {
    capture_output(|| {
        replay_text_dump(
            entries,
            after_seq,
            &HashMap::new(),
            crate::new_text_dump_sink(),
        )
        .unwrap();
    })
}

pub(super) fn dump_in_batches(entries: &[(u64, SessionLogEntry)], cut_points: &[usize]) -> String {
    capture_output(|| {
        let sink = crate::new_text_dump_sink();
        let mut after_seq = 0;
        for &end in cut_points {
            replay_text_dump(&entries[..end], after_seq, &HashMap::new(), sink.clone()).unwrap();
            after_seq = end as u64;
        }
    })
}

pub(super) fn assert_exported_entries(entries: &[(u64, SessionLogEntry)], format: SessionFormat) {
    let output = dump_format(entries, format);
    let actual: Vec<SessionLogEntry> = match format {
        SessionFormat::Json => output
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect(),
        SessionFormat::Yaml => serde_yaml::Deserializer::from_str(&output)
            .map(|doc| SessionLogEntry::deserialize(doc).unwrap())
            .collect(),
        _ => panic!("unsupported format for structured entry assertion"),
    };
    let expected: Vec<_> = entries.iter().map(|(_, entry)| entry.clone()).collect();
    assert_eq!(actual, expected);
}

#[test]
fn text_dump_output_matches_shared_compaction_fixture() {
    insta::assert_snapshot!("compacted_dump", dump(&fixture()));
}

#[test]
fn text_dump_output_with_zero_kept_is_summary_only() {
    assert_eq!(
        dump(&fixture_through(6)),
        format!("── Compaction 5: harnx dump session plain history --before-compaction 5 ──\n{EMPTY_SUMMARY}\n\n")
    );
}

#[test]
fn text_dump_output_without_compaction_is_unchanged() {
    assert_eq!(
        dump(&fixture_through(4)),
        "Inspect the session log format.\nTool calls and results are separate log entries.\n"
    );
}

#[test]
fn text_dump_follow_emits_only_later_activity() {
    assert_eq!(
        dump_after(&fixture(), 20),
        "Show the previous compaction's history.\nThe earlier window contains the original prompt and both tool rounds.\n"
    );
}

#[test]
fn text_dump_follow_starts_new_view_at_new_compaction() {
    assert_eq!(dump_after(&fixture(), 14), dump(&fixture()));
    assert_eq!(
        dump_after(&fixture_through(6), 4),
        dump(&fixture_through(6))
    );
}

#[test]
fn text_dump_follow_keeps_one_marker_across_split_tool_round() {
    let entries = fixture();
    let output = dump_in_batches(&entries, &[15, 17, 18, 20, 23]);
    assert_eq!(output, dump(&entries));
}

#[test]
fn text_dump_resolves_replacement_rows_with_shared_out_of_order_sequences() {
    let mut entries = fixture();
    let expected = dump(&entries).replace(
        "Implement the shared transcript view and verify paging.\n",
        &dump_after(&entries, 20),
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
    assert_eq!(dump(&entries), expected);
}

#[test]
fn text_dump_resolves_rewind_before_selecting_marker() {
    let mut entries = fixture();
    entries.push((24, SessionLogEntry::Rewind { after_seq: 6 }));
    assert_eq!(dump(&entries), dump(&fixture_through(6)));
}

#[test]
fn text_dump_follow_without_new_entries_is_silent() {
    assert_eq!(dump_after(&fixture(), 23), "");
}

#[test]
fn json_dump_preserves_durable_entries_and_raw_summaries() {
    assert_exported_entries(&fixture(), SessionFormat::Json);
}

#[test]
fn yaml_dump_preserves_durable_entries_and_raw_summaries() {
    assert_exported_entries(&fixture(), SessionFormat::Yaml);
}

mod controls;
mod history;
