//! What a tool server declares about how its result reads in a script.

/// A tool's script-facing declarations, from its spec `_meta`.
#[derive(Clone, Debug, Default)]
pub struct ScriptShape {
    /// Which result field is stdout, stderr and the exit status.
    pub output: Option<harnx_toolset::ScriptOutput>,
    /// JSON Schema of the structured result, listed by `help <tool>`.
    pub output_schema: Option<serde_json::Value>,
}
