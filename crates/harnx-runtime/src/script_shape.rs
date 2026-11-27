//! How a tool's result reads in a script.

/// Script-only declarations supplied by a built-in tool or a server spec's `_meta`.
#[derive(Clone, Debug, Default)]
pub struct ScriptShape {
    /// Which result field is stdout, stderr and the exit status.
    pub output: Option<harnx_toolset::ScriptOutput>,
    /// JSON Schema of the structured result, listed by `help <tool>`.
    pub output_schema: Option<serde_json::Value>,
}
