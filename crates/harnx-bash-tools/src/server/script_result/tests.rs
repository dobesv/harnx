use super::*;
use serde_json::Value;

fn report<'a>(stdout: &'a str, stderr: &'a str, cap: usize) -> CommandReport<'a> {
    CommandReport {
        execution_id: "exec-1",
        status: "exited",
        exit_code: 0,
        stdout,
        stderr,
        stdout_log_path: "/logs/out",
        stderr_log_path: "/logs/err",
        max_output_bytes: cap,
        log_command: Some("bash_read_exec_log".to_string()),
        log_unfiltered: true,
        notes: Vec::new(),
        diff: None,
    }
}

fn fields(report: CommandReport<'_>) -> Value {
    command_result(report, "summary".to_string())
        .structured_content
        .expect("structured content")
}

#[cfg(unix)]
fn raw_status(raw: i32) -> ExitStatus {
    std::os::unix::process::ExitStatusExt::from_raw(raw)
}

#[cfg(unix)]
#[test]
fn exited_reports_the_commands_code_with_no_notes() {
    let ended = ended(ExitKind::Exited, raw_status(3 << 8));
    assert_eq!(ended.status, "exited");
    assert_eq!(ended.exit_code, 3);
    assert!(ended.notes.is_empty());
}

#[cfg(unix)]
#[test]
fn signal_ended_reports_128_plus_n_and_a_note() {
    let ended = ended(ExitKind::Exited, raw_status(15));
    assert_eq!(ended.status, "exited");
    assert_eq!(ended.exit_code, 143);
    assert_eq!(ended.notes, ["ended by signal 15"]);
}

#[cfg(unix)]
#[test]
fn timeout_reports_124_whatever_the_status() {
    let ended = ended(ExitKind::TimedOut { deadline_secs: 7 }, raw_status(9));
    assert_eq!(ended.status, "timeout");
    assert_eq!(ended.exit_code, 124);
    assert_eq!(ended.notes, ["timed out after 7s and was terminated"]);
}

#[cfg(unix)]
#[test]
fn interruption_reports_130() {
    let ended = ended(ExitKind::Interrupted, raw_status(15));
    assert_eq!(ended.status, "interrupted");
    assert_eq!(ended.exit_code, 130);
    assert_eq!(ended.notes, ["stopped by a new message"]);
}

#[cfg(unix)]
#[test]
fn shell_status_of_a_signal_is_128_plus_n() {
    let status = std::process::Command::new("sh")
        .args(["-c", "kill -9 $$"])
        .status()
        .expect("run sh");
    assert_eq!(shell_status(status), 137);
}

#[test]
fn stream_over_the_cap_keeps_whole_lines_and_notes_it() {
    let fields = fields(report("aaaa\nbbbb\ncccc\n", "", 12));
    assert_eq!(fields["stdout"], "aaaa\nbbbb\n");
    assert_eq!(
        fields["notes"][0],
        "stdout cut at 10 of 15 bytes; full log: bash_read_exec_log --execution-id exec-1 --stream stdout --offset 3"
    );
}

#[test]
fn each_stream_is_cut_on_its_own_and_templates_name_no_log_tool() {
    let mut input = report("short\n", "eeee\nffff\n", 6);
    input.log_command = None;
    let fields = fields(input);
    assert_eq!(fields["stdout"], "short\n");
    assert_eq!(fields["stderr"], "eeee\n");
    assert_eq!(
        fields["notes"],
        serde_json::json!(["stderr cut at 5 of 10 bytes"])
    );
}

#[test]
fn a_single_line_longer_than_the_cap_is_cut_with_a_note() {
    let fields = fields(report("héllo wörld", "", 2));
    // The cap falls inside the two-byte `é`, so the cut backs up to `h`.
    assert_eq!(fields["stdout"], "h");
    assert!(fields["notes"][0]
        .as_str()
        .unwrap()
        .starts_with("stdout cut at 1 of 13 bytes"));
}

#[test]
fn empty_streams_are_empty_strings_not_missing() {
    let fields = fields(report("", "", 100));
    assert_eq!(fields["stdout"], "");
    assert_eq!(fields["stderr"], "");
    assert_eq!(fields["notes"], serde_json::json!([]));
}

#[test]
fn exec_mapping_names_the_result_fields() {
    let mapping = exec_script_output();
    assert_eq!(mapping.stdout.as_deref(), Some("stdout"));
    assert_eq!(mapping.stderr_text.as_deref(), Some("stderr"));
    assert_eq!(mapping.stderr.as_deref(), Some("notes"));
    assert_eq!(mapping.exit_code.as_deref(), Some("exit_code"));
    assert_eq!(mapping.error_exit_code, Some(125));
}
