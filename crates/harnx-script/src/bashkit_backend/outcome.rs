use bashkit::{Error, ExecResult, LimitExceeded};

use crate::{ScriptRun, ScriptStatus};

use super::{capture::Capture, INTERRUPTED_EXIT_CODE};

type Outcome = (ScriptStatus, i32, Option<String>);

pub(super) fn finish(
    result: bashkit::Result<ExecResult>,
    capture: &mut Capture,
    fatal: Option<String>,
    host_stopped: bool,
) -> ScriptRun {
    let (status, exit_code, limit_hit) = if host_stopped {
        (
            ScriptStatus::Stopped,
            INTERRUPTED_EXIT_CODE,
            capture.output_limit(),
        )
    } else if let Some(message) = fatal {
        capture.record_error(message);
        (ScriptStatus::Error, 1, capture.output_limit())
    } else {
        classify(result, capture)
    };
    ScriptRun {
        stdout: std::mem::take(&mut capture.stdout.text),
        stderr: std::mem::take(&mut capture.stderr.text),
        stdout_truncated: capture.stdout.truncated,
        stderr_truncated: capture.stderr.truncated,
        exit_code,
        status,
        limit_hit,
        error: capture.error.take(),
    }
}

pub(super) fn classify(result: bashkit::Result<ExecResult>, capture: &mut Capture) -> Outcome {
    let (status, code, limit) = match result {
        Ok(result) => classify_success(result, capture),
        Err(error) => classify_error(error, capture),
    };
    (status, code, limit.or_else(|| capture.output_limit()))
}

fn classify_success(result: ExecResult, capture: &mut Capture) -> Outcome {
    // Streaming preserves partial output on errors; the final result is
    // authoritative on success (not every compound command streams).
    capture.replace(result);
    let status = if capture.exit_code == 0 {
        ScriptStatus::Ok
    } else {
        ScriptStatus::Error
    };
    (status, capture.exit_code, None)
}

fn classify_error(error: Error, capture: &mut Capture) -> Outcome {
    capture.record_error(error.to_string());
    match error {
        Error::Cancelled => (ScriptStatus::Cancelled, INTERRUPTED_EXIT_CODE, None),
        Error::ResourceLimit(limit) => {
            let (status, code) = if matches!(limit, LimitExceeded::Timeout(_)) {
                (ScriptStatus::TimedOut, 124)
            } else {
                (ScriptStatus::Error, 1)
            };
            (status, code, Some(limit.to_string()))
        }
        Error::Parse { .. } => (ScriptStatus::Error, 2, None),
        _ => (ScriptStatus::Error, 1, None),
    }
}
