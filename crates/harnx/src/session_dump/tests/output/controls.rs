use super::*;

struct ControlCase {
    name: &'static str,
    input: &'static str,
    visible: &'static str,
}

const CASES: &[ControlCase] = &[
    ControlCase {
        name: "CSI",
        input: "before\x1b[2Jafter",
        visible: "before[2Jafter",
    },
    ControlCase {
        name: "OSC52 BEL",
        input: "before\x1b]52;c;SGVsbG8=\x07after",
        visible: "before]52;c;SGVsbG8=after",
    },
    ControlCase {
        name: "OSC52 ST",
        input: "before\x1b]52;c;SGVsbG8=\x1b\\after",
        visible: "before]52;c;SGVsbG8=\\after",
    },
    ControlCase {
        name: "BEL",
        input: "before\x07after",
        visible: "beforeafter",
    },
    ControlCase {
        name: "CR",
        input: "before\rafter",
        visible: "beforeafter",
    },
    ControlCase {
        name: "DEL and backspace",
        input: "before\x7f\x08after",
        visible: "beforeafter",
    },
    ControlCase {
        name: "C1",
        input: "before\u{009b}2J\u{009d}52;c;SGVsbG8=\u{009c}after",
        visible: "before2J52;c;SGVsbG8=after",
    },
    ControlCase {
        name: "layout",
        input: "Summary\n\tUnicode café — 保留\r\nDone",
        visible: "Summary\n\tUnicode café — 保留\nDone",
    },
];

fn summary_entries(summary: &str) -> Vec<(u64, SessionLogEntry)> {
    let mut entries = fixture();
    entries[14].1 = SessionLogEntry::Compress {
        prompt: summary.into(),
    };
    entries
}

fn assert_safe_summary(output: &str, case: &ControlCase, marker_seq: u64) {
    assert!(
        output
            .chars()
            .all(|ch| !ch.is_control() || matches!(ch, '\n' | '\t')),
        "{}: {output:?}",
        case.name
    );
    let prefix = format!(
        "── Compaction {marker_seq}: harnx dump session plain history --before-compaction {marker_seq} ──\n{}\n\n",
        case.visible
    );
    assert!(output.starts_with(&prefix), "{}: {output:?}", case.name);
}

#[test]
fn dump_summary_strips_terminal_controls() {
    for case in CASES {
        assert_safe_summary(&dump(&summary_entries(case.input)), case, 15);
    }
}

#[test]
fn follow_summary_strips_terminal_controls() {
    for case in CASES {
        assert_safe_summary(&dump_after(&summary_entries(case.input), 14), case, 15);
    }
}

#[test]
fn follow_batches_sanitize_new_marker_without_changing_later_output() {
    let entries = summary_entries("Summary\n\t\x1b[2J\x1b]52;c;SGVsbG8=\x07\rDone");
    let expected = format!("{}{}", dump(&entries[..4]), dump(&entries));
    let actual = dump_in_batches(&entries, &[4, 15, 23]);
    assert_eq!(actual, expected);
    assert!(actual.contains("Summary\n\t[2J]52;c;SGVsbG8=Done\n\n"));
}

#[test]
fn terminal_sanitization_does_not_change_raw_summary_or_structured_exports() {
    let entries = summary_entries("Summary\x1b]52;c;SGVsbG8=\x07\r\n\tDone");
    let original = entries.clone();
    dump(&entries);
    assert_eq!(entries, original);
    assert_exported_entries(&entries, SessionFormat::Json);
}

#[test]
fn live_info_preserves_renderer_generated_ansi() {
    let sink = crate::cli_event_sink::CliAgentEventSink::new(
        false,
        harnx_render::RenderOptions::default(),
        harnx_core::abort::create_abort_signal(),
    );
    let text = "\x1b[2mstatus\x1b[0m";
    let output = capture_output(|| {
        sink.emit(AgentEvent::Notice(NoticeEvent::Info(text.into())));
    });
    assert_eq!(output, format!("{text}\n"));
}

#[test]
fn archived_summary_strips_terminal_controls_without_changing_the_log() {
    let args = parse_dump("harnx dump session plain history --before-compaction 15");
    for case in CASES {
        let mut entries = fixture();
        entries[4].1 = SessionLogEntry::Compress {
            prompt: case.input.into(),
        };
        let original = entries.clone();
        assert_safe_summary(&dump_with_args(&entries, &args), case, 5);
        assert_eq!(entries, original);
    }
}
