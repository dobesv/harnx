//! Partial results: what a tool call that does not succeed still hands back.
//!
//! A tool records a partial result while it runs
//! (`harnx_toolset::ToolInvocationContext::record_partial_result`). When the
//! call fails, times out, is interrupted or loses its response, whoever writes
//! its output adds the latest value under [`PARTIAL_RESULT_KEY`]. A successful
//! call returns only its own result.

use serde_json::Value;
use std::fmt;

/// Key a non-success tool output carries its partial result under.
pub const PARTIAL_RESULT_KEY: &str = "partial_result";

/// A failed call's error together with the partial result its tool recorded.
/// The message is the original error's whole chain, so displaying it reads
/// exactly as the original did.
#[derive(Debug)]
struct PartialResultError {
    message: String,
    partial_result: Value,
}

impl fmt::Display for PartialResultError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PartialResultError {}

/// Attach `partial_result` to a failed call's error.
pub fn error_with_partial_result(error: anyhow::Error, partial_result: Value) -> anyhow::Error {
    anyhow::Error::new(PartialResultError {
        message: format!("{error:#}"),
        partial_result,
    })
}

/// The partial result attached to `error`, looking through any context added
/// on top of it.
pub fn partial_result_of(error: &anyhow::Error) -> Option<&Value> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<PartialResultError>())
        .map(|failure| &failure.partial_result)
}

/// Add `partial_result` to a non-success tool output. An output that is not a
/// JSON object has nowhere to put it and comes back unchanged.
pub fn output_with_partial_result(mut output: Value, partial_result: Option<&Value>) -> Value {
    if let (Some(object), Some(partial_result)) = (output.as_object_mut(), partial_result) {
        object.insert(PARTIAL_RESULT_KEY.to_string(), partial_result.clone());
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_partial_result_survives_added_context() {
        let error =
            error_with_partial_result(anyhow::anyhow!("child failed"), json!({"session_id": "s1"}))
                .context("tool call failed");

        assert_eq!(
            partial_result_of(&error),
            Some(&json!({"session_id": "s1"}))
        );
    }

    #[test]
    fn a_plain_error_has_no_partial_result() {
        assert_eq!(partial_result_of(&anyhow::anyhow!("boom")), None);
    }

    #[test]
    fn attaching_keeps_the_whole_error_message() {
        let error = anyhow::anyhow!("inner").context("outer");
        let error = error_with_partial_result(error, json!({}));

        assert_eq!(format!("{error:#}"), "outer: inner");
        assert_eq!(error.to_string(), "outer: inner");
    }

    #[test]
    fn only_object_outputs_gain_a_partial_result() {
        let partial = json!({"session_id": "s1"});

        assert_eq!(
            output_with_partial_result(json!({"is_error": true, "error": "x"}), Some(&partial)),
            json!({"is_error": true, "error": "x", "partial_result": {"session_id": "s1"}})
        );
        assert_eq!(
            output_with_partial_result(json!("text"), Some(&partial)),
            json!("text")
        );
        assert_eq!(
            output_with_partial_result(json!({"error": "x"}), None),
            json!({"error": "x"})
        );
    }
}
