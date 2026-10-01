//! Loop protection shared by the runtime and the CLI.
//!
//! A model stuck in a repetition loop keeps requesting the same tool call, or
//! keeps streaming the same text, while the harness pays for every round.
//! `tool_repeat` judges tool calls and `output_repeat` judges a reply's answer
//! and thinking. The decision logic here is pure and takes its clock from the
//! caller, so a live turn and a replay of a stored session reach the same
//! verdicts.

mod digest;
pub mod output_repeat;
pub mod replay;
pub mod tool_repeat;

pub use output_repeat::{
    check_output, detect_in_text, find_repetitive_output, OutputChannel, OutputRepeatDetector,
    RepeatedTail, RepetitiveOutput, OUTPUT_REPEAT_MAX_UNIT, OUTPUT_REPEAT_MIN_COPIES,
    OUTPUT_REPEAT_MIN_COVER,
};
pub use tool_repeat::{
    append_note, strip_note, Refusal, RepeatNote, ToolRepeatGuard, ToolRepeatVerdict,
    TOOL_REPEAT_LIMIT, TOOL_REPEAT_WINDOW,
};

use serde::{Deserialize, Serialize};

/// Prefix of the marker a repetition stop leaves in the turn's error text.
/// [`parse_repetition_terminal`] is the only code that may match it.
const REPETITION_TERMINAL_PREFIX: &str = "harnx:repetition ";

/// What kept repeating.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RepetitionSource {
    ToolCalls,
    Answer,
    Thinking,
}

/// Machine-readable details of a repetition stop.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RepetitionTerminal {
    pub source: RepetitionSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<usize>,
}

impl RepetitionTerminal {
    pub fn output(source: RepetitionSource) -> Self {
        Self {
            source,
            tool: None,
            count: None,
        }
    }

    pub fn tool_calls(tool: &str, count: usize) -> Self {
        Self {
            source: RepetitionSource::ToolCalls,
            tool: Some(tool.to_string()),
            count: Some(count),
        }
    }

    /// Why the turn stopped, as a clause.
    pub fn reason(&self) -> String {
        match self.source {
            RepetitionSource::Answer => {
                "the model's reply kept repeating the same text".to_string()
            }
            RepetitionSource::Thinking => {
                "the model's reasoning kept repeating the same text".to_string()
            }
            RepetitionSource::ToolCalls => format!(
                "the model kept repeating the same `{}` call with identical arguments and results",
                self.tool.as_deref().unwrap_or("tool")
            ),
        }
    }

    pub fn sentence(&self) -> String {
        format!("Stopped: {}.", self.reason())
    }
}

/// The error that ends a turn stopped for repetition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepetitionStop(pub RepetitionTerminal);

// The worker persists a failed turn as the error's text, and parents recover
// the details from that text, so the marker has to be part of `Display`.
impl std::fmt::Display for RepetitionStop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let payload = serde_json::to_string(&self.0).map_err(|_| std::fmt::Error)?;
        write!(
            f,
            "{} {REPETITION_TERMINAL_PREFIX}{payload}",
            self.0.sentence()
        )
    }
}

impl std::error::Error for RepetitionStop {}

/// Recover the details of a repetition stop from a persisted error message.
pub fn parse_repetition_terminal(message: &str) -> Option<RepetitionTerminal> {
    let start = message.rfind(REPETITION_TERMINAL_PREFIX)?;
    serde_json::from_str(&message[start + REPETITION_TERMINAL_PREFIX.len()..]).ok()
}

/// The readable sentence of a repetition stop anywhere in `error`'s chain.
pub fn repetition_stop_sentence(error: &anyhow::Error) -> Option<String> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<RepetitionStop>())
        .map(|stop| stop.0.sentence())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fs_read_stop() -> RepetitionTerminal {
        RepetitionTerminal::tool_calls("fs_read", 4)
    }

    #[test]
    fn stop_text_is_the_sentence_then_the_marker() {
        let text = RepetitionStop(fs_read_stop()).to_string();
        assert_eq!(
            text,
            "Stopped: the model kept repeating the same `fs_read` call with identical \
             arguments and results. harnx:repetition \
             {\"source\":\"tool_calls\",\"tool\":\"fs_read\",\"count\":4}"
        );
    }

    #[test]
    fn marker_round_trips_even_when_wrapped_in_context() {
        let error = anyhow::Error::new(RepetitionStop(fs_read_stop())).context("worker turn");
        let persisted = format!("{error:#}");
        assert_eq!(parse_repetition_terminal(&persisted), Some(fs_read_stop()));
    }

    #[test]
    fn plain_and_malformed_errors_are_not_repetition_stops() {
        assert_eq!(parse_repetition_terminal("worker failed"), None);
        assert_eq!(parse_repetition_terminal("harnx:repetition not-json"), None);
    }

    #[test]
    fn sentence_is_found_anywhere_in_the_chain() {
        let error = anyhow::Error::new(RepetitionStop(fs_read_stop())).context("outer");
        assert_eq!(
            repetition_stop_sentence(&error).as_deref(),
            Some("Stopped: the model kept repeating the same `fs_read` call with identical arguments and results.")
        );
        assert_eq!(repetition_stop_sentence(&anyhow::anyhow!("other")), None);
    }
}
