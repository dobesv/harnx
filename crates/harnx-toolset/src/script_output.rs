//! How a tool's structured result reads inside a `bash` script.
//!
//! A tool opts in by declaring a [`ScriptOutput`] mapping in its spec `_meta`
//! and the script layer asks for the structured result with
//! [`CAPABILITY_STRUCTURED_RESULT`].

use serde::{Deserialize, Serialize};

/// Request capability a `bash` script's inner calls send: the caller maps the
/// result's `structuredContent` itself and wants it present.
pub const CAPABILITY_STRUCTURED_RESULT: &str = "harnx:structured_result";

/// `_meta` key holding a tool's [`ScriptOutput`].
pub const SCRIPT_OUTPUT_META_KEY: &str = "harnx:script_output";

/// Same fields and meaning as `harnx_script::commands::OutputMapping`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptOutput {
    /// structuredContent field printed on stdout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    /// Format for an object item of the stdout field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row: Option<String>,
    /// structuredContent string field printed on stderr exactly as stored,
    /// before the notes. For a command's own stderr.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_text: Option<String>,
    /// structuredContent field printed on stderr.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
    /// structuredContent integer field used as the exit status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<String>,
    /// Exit status for a tool error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_exit_code: Option<i32>,
    /// One line for `help <tool>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// Notes for `head_lines` and `tail_lines` passed to a tool in a script, where
/// the tool cannot apply them and a pipe to `head` or `tail` does the job.
pub fn line_cap_notes(head: Option<usize>, tail: Option<usize>) -> Vec<String> {
    [("head_lines", "head", head), ("tail_lines", "tail", tail)]
        .into_iter()
        .filter(|(_, _, value)| value.is_some())
        .map(|(name, command, _)| format!("{name} is ignored in a script; pipe to {command}"))
        .collect()
}
