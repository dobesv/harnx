use super::*;
use crate::cli::{Cli, Commands, DumpSubcommands};
use clap::Parser;
use harnx_core::event::{AgentEvent, NoticeEvent};
use std::sync::Mutex;

pub(crate) fn parse_dump(command: &str) -> DumpSessionArgs {
    let cli = Cli::try_parse_from(shell_words::split(command).unwrap()).unwrap();
    let Some(Commands::Dump(args)) = cli.command else {
        panic!("expected dump command");
    };
    let DumpSubcommands::Session(args) = args.command else {
        panic!("expected dump session");
    };
    args.validate().unwrap();
    args
}

pub(crate) fn dump_args() -> DumpSessionArgs {
    parse_dump("harnx dump session plain history")
}

fn replay_text_dump(
    entries: &[(u64, SessionLogEntry)],
    after_seq: u64,
    declarations: &HashMap<String, ToolDeclaration>,
    sink: Arc<dyn AgentEventSink>,
) -> Result<()> {
    DumpRenderer::new(&dump_args(), declarations.clone(), sink).replay(entries, after_seq)
}

const SUMMARY: &str = "Earlier log inspection is archived. The latest prompt, verification round and final reply remain verbatim.";
const EMPTY_SUMMARY: &str =
    "The session log stores tool calls and results separately. No messages were kept.";

pub(crate) fn fixture() -> Vec<(u64, SessionLogEntry)> {
    serde_json::from_str(include_str!(
        "../../../harnx-core/tests/fixtures/compaction-history.json"
    ))
    .unwrap()
}

fn fixture_through(seq: u64) -> Vec<(u64, SessionLogEntry)> {
    fixture().into_iter().filter(|(n, _)| *n <= seq).collect()
}

#[derive(Default)]
struct RecordingSink(Mutex<Vec<AgentEvent>>);

impl AgentEventSink for RecordingSink {
    fn emit(&self, event: AgentEvent) {
        self.0.lock().unwrap().push(event);
    }
}

fn record(action: impl FnOnce(Arc<dyn AgentEventSink>)) -> Vec<serde_json::Value> {
    let sink = Arc::new(RecordingSink::default());
    action(sink.clone());
    let events = sink.0.lock().unwrap();
    events
        .iter()
        .map(|event| serde_json::to_value(event).unwrap())
        .collect()
}

fn marker_event(seq: u64, summary: &str) -> serde_json::Value {
    serde_json::to_value(AgentEvent::Notice(NoticeEvent::Info(format!(
        "── Compaction {seq}: harnx dump session plain history --before-compaction {seq} ──\n{summary}\n"
    ))))
    .unwrap()
}

#[test]
fn text_dump_fixture_has_marker_then_kept_and_later_events_once() {
    let entries = fixture();
    let actual = record(|sink| {
        replay_text_dump(&entries, 0, &HashMap::new(), sink).unwrap();
    });
    let mut expected = record(|sink| {
        harnx_runtime::replay_entries_to_sink(&entries[15..], sink);
    });
    expected.insert(0, marker_event(15, SUMMARY));
    assert_eq!(actual, expected);
}

#[test]
fn text_dump_zero_kept_still_has_summary() {
    let actual = record(|sink| {
        replay_text_dump(&fixture_through(6), 0, &HashMap::new(), sink).unwrap();
    });
    assert_eq!(actual, vec![marker_event(5, EMPTY_SUMMARY)]);
}

#[test]
fn text_dump_uncompacted_events_are_unchanged() {
    let entries = fixture_through(4);
    let actual = record(|sink| replay_text_dump(&entries, 0, &HashMap::new(), sink).unwrap());
    let expected = record(|sink| harnx_runtime::replay_entries_to_sink(&entries, sink));
    assert_eq!(actual, expected);
}

#[test]
fn text_dump_does_not_change_low_level_replay_contract() {
    let entries = fixture();
    let actual = record(|sink| harnx_runtime::replay_entries_to_sink(&entries, sink));
    let prompt = "Implement the shared transcript view and verify paging.";
    assert_eq!(
        actual
            .iter()
            .filter(|event| event.to_string().contains(prompt))
            .count(),
        2
    );
}

#[test]
fn text_dump_rejects_invalid_log_before_emitting_output() {
    let entries = vec![(0, SessionLogEntry::Clear)];
    let actual = record(|sink| {
        let result = replay_text_dump(&entries, 0, &HashMap::new(), sink);
        assert!(result.is_err());
    });
    assert!(actual.is_empty());
}

#[cfg(unix)]
mod output;

#[test]
fn before_compaction_rejects_structured_formats_before_session_io() {
    for format in [SessionFormat::Json, SessionFormat::Yaml] {
        let args = DumpSessionArgs {
            format,
            before_compaction: Some("15".parse().unwrap()),
            ..dump_args()
        };
        let command = crate::cli::DumpArgs {
            command: DumpSubcommands::Session(args),
        };
        let error = tokio_test::block_on(crate::run_dump_command(&command)).unwrap_err();
        assert_eq!(
            error.to_string(),
            "--before-compaction supports only --format text; JSON/YAML export the full log"
        );
    }
}

#[test]
fn shared_default_adapter_preserves_non_cli_marker_and_cursor() {
    let entries = fixture();
    let mut expected = record(|sink| harnx_runtime::replay_entries_to_sink(&entries[15..], sink));
    let marker = AgentEvent::Notice(NoticeEvent::Info(format!(
        "── Compaction 15: earlier history archived ──\n{SUMMARY}\n"
    )));
    expected.insert(0, serde_json::to_value(marker).unwrap());
    for cursor in [0, 14] {
        let actual = record(|sink| {
            harnx_runtime::replay_default_view_to_sink(&entries, cursor, &HashMap::new(), sink)
                .unwrap()
        });
        assert_eq!(actual, expected);
    }
}

#[test]
fn shared_default_adapter_keeps_already_emitted_marker_silent() {
    let entries = fixture();
    let actual = record(|sink| {
        harnx_runtime::replay_default_view_to_sink(&entries, 20, &HashMap::new(), sink).unwrap()
    });
    let expected = record(|sink| harnx_runtime::replay_entries_to_sink(&entries[20..], sink));
    assert_eq!(actual, expected);
}
