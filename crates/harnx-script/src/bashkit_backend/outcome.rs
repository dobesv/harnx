use bashkit::{Error, ExecResult, LimitExceeded};

use crate::ScriptStatus;

use super::capture::Capture;

type Outcome = (ScriptStatus, i32, Option<String>);

pub(super) fn classify(result: bashkit::Result<ExecResult>, capture: &mut Capture) -> Outcome {
    match result {
        Ok(result) => classify_success(result, capture),
        Err(error) => classify_error(error, capture),
    }
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
    let limit = if capture.stdout.truncated || capture.stderr.truncated {
        Some("output capture limit exceeded".to_owned())
    } else {
        None
    };
    (status, capture.exit_code, limit)
}

fn classify_error(error: Error, capture: &mut Capture) -> Outcome {
    capture.append("", &format!("{error}\n"));
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
