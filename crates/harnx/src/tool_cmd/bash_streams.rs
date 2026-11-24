use harnx_runtime::operator_tools::OperatorToolReply;

/// The exit status `timeout(1)` uses when it stops a command.
const TIMED_OUT_EXIT_CODE: u8 = 124;
/// 128 + SIGINT, the status a shell reports for an interrupted command.
const CANCELLED_EXIT_CODE: u8 = 130;

#[derive(Debug, PartialEq, Eq)]
struct BashStreams {
    stdout: String,
    stderr: String,
    exit_code: u8,
}

fn flag(payload: &serde_json::Value, key: &str) -> bool {
    payload[key].as_bool().unwrap_or(false)
}

fn script_exit_code(payload: &serde_json::Value) -> Option<u8> {
    let code = u8::try_from(payload["exit_code"].as_i64()?).ok()?;
    Some(code)
}

/// A script that ran to its own `exit N` reports `ok` for 0 and `error` with
/// no payload `error` text for N > 0; anything else (timeout, cancellation, a
/// parse or resource error) is something the script did not choose.
fn script_just_exited(status: &str, payload: &serde_json::Value) -> bool {
    status == "ok" || (status == "error" && payload.get("error").is_none())
}

/// Whether `error` is the tool result's own error, as the worker derives it
/// from the output. Anything else (reservation cleanup, transport) says
/// nothing about the script and must not be swallowed.
fn is_result_own_error(output: &serde_json::Value, error: &str) -> bool {
    let failed = ["isError", "is_error", "blocked_by_hook"]
        .iter()
        .any(|key| output[key].as_bool() == Some(true));
    failed && error == output["error"].as_str().unwrap_or("Tool reported an error")
}

/// `None` when `output` isn't a bash payload (a reservation or startup
/// failure), so the caller falls back to the generic printing.
fn bash_streams(output: &str, error: Option<&str>) -> Option<BashStreams> {
    let value: serde_json::Value = serde_json::from_str(output).ok()?;
    let payload = value
        .get("partial_result")
        .filter(|partial| partial.is_object())
        .unwrap_or(&value);
    let stdout = payload["stdout"].as_str()?.to_owned();
    let mut stderr = payload["stderr"].as_str().unwrap_or_default().to_owned();
    for stream in ["stdout", "stderr"] {
        if flag(payload, &format!("{stream}_truncated")) {
            stderr.push_str(&format!("harnx: {stream} truncated\n"));
        }
    }
    let status = payload["status"].as_str().unwrap_or_default();
    let just_exited = script_just_exited(status, payload);
    let foreign = error.is_some_and(|error| !is_result_own_error(&value, error));
    if let Some(error) = error.filter(|_| foreign || !just_exited) {
        stderr.push_str(&format!("error: {error}\n"));
    }
    let exit_code = if just_exited {
        let code = script_exit_code(payload).unwrap_or(1);
        // A failure the script did not cause must not read as success.
        if foreign && code == 0 {
            1
        } else {
            code
        }
    } else {
        match status {
            "timed_out" => TIMED_OUT_EXIT_CODE,
            "cancelled" => CANCELLED_EXIT_CODE,
            // 0 would report a script that never finished as a success.
            _ => script_exit_code(payload)
                .filter(|code| *code != 0)
                .unwrap_or(1),
        }
    };
    Some(BashStreams {
        stdout,
        stderr,
        exit_code,
    })
}

/// Print a bash reply the way `bash -c` would. `None` leaves the reply to the
/// generic printing.
pub(super) fn print(reply: &OperatorToolReply) -> Option<u8> {
    let streams = bash_streams(&reply.output, reply.error.as_deref())?;
    print!("{}", streams.stdout);
    eprint!("{}", streams.stderr);
    Some(streams.exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(status: &str, exit_code: i64) -> serde_json::Value {
        serde_json::json!({
            "status": status, "exit_code": exit_code,
            "stdout": "out\n", "stderr": "err\n",
            "stdout_truncated": false, "stderr_truncated": false,
        })
    }

    #[test]
    fn bash_streams_pass_output_and_exit_status_through() {
        let ok = bash_streams(&payload("ok", 0).to_string(), None).unwrap();
        assert_eq!(
            ok,
            BashStreams {
                stdout: "out\n".into(),
                stderr: "err\n".into(),
                exit_code: 0
            }
        );

        // What the runtime produces for `exit 3`: status "error", no payload
        // `error`, and the wrapper's error equal to the reply's error.
        let message = "bash script failed with exit code 3";
        let failed = serde_json::json!({
            "is_error": true,
            "error": message,
            "partial_result": payload("error", 3),
        });
        let failed = bash_streams(&failed.to_string(), Some(message)).unwrap();
        assert_eq!(failed.exit_code, 3);
        assert_eq!(
            failed.stderr, "err\n",
            "a plain non-zero exit adds no error line"
        );
    }

    #[test]
    fn bash_streams_keep_errors_the_script_did_not_cause() {
        let cleanup = "Tool reservation cleanup failed: x";
        let streams = bash_streams(&payload("ok", 0).to_string(), Some(cleanup)).unwrap();
        assert_eq!(streams.stdout, "out\n");
        assert_eq!(
            streams.stderr,
            "err\nerror: Tool reservation cleanup failed: x\n"
        );
        assert_eq!(streams.exit_code, 1);

        let mut broken = payload("error", 1);
        broken["error"] = "parse error".into();
        let wrapper =
            serde_json::json!({"is_error": true, "error": "parse error", "partial_result": broken});
        let streams = bash_streams(&wrapper.to_string(), Some("parse error")).unwrap();
        assert_eq!(streams.exit_code, 1);
        assert_eq!(streams.stderr, "err\nerror: parse error\n");
    }

    #[test]
    fn bash_streams_report_what_the_script_did_not_cause() {
        let mut truncated = payload("ok", 0);
        truncated["stdout_truncated"] = true.into();
        let streams = bash_streams(&truncated.to_string(), None).unwrap();
        assert_eq!(streams.stderr, "err\nharnx: stdout truncated\n");

        let mut truncated = payload("ok", 0);
        truncated["stderr_truncated"] = true.into();
        let streams = bash_streams(&truncated.to_string(), None).unwrap();
        assert_eq!(streams.stderr, "err\nharnx: stderr truncated\n");

        let timed_out =
            serde_json::json!({"is_error": true, "partial_result": payload("timed_out", 0)});
        let streams = bash_streams(
            &timed_out.to_string(),
            Some("bash script timed out after 5s"),
        )
        .unwrap();
        assert_eq!(streams.exit_code, 124);
        assert_eq!(
            streams.stderr,
            "err\nerror: bash script timed out after 5s\n"
        );

        let cancelled = serde_json::json!({"partial_result": payload("cancelled", 0)});
        assert_eq!(
            bash_streams(&cancelled.to_string(), None)
                .unwrap()
                .exit_code,
            130
        );
    }

    #[test]
    fn bash_streams_leave_other_replies_to_the_generic_path() {
        assert_eq!(
            bash_streams("", Some("Tool reservation startup timed out")),
            None
        );
        assert_eq!(
            bash_streams(r#"{"isError":true,"error":"no such tool"}"#, None),
            None
        );
        assert_eq!(bash_streams("not json", None), None);
    }
}
