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
