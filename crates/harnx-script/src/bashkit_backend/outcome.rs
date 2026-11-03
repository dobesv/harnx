use bashkit::{Error, ExecResult, LimitExceeded};

use crate::ScriptStatus;

use super::capture::Capture;

type Outcome = (ScriptStatus, i32, Option<String>);

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
        Error::Cancelled => (ScriptStatus::Cancelled, 130, None),
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
