use harnx_core::hooks::HooksConfig;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

fn default_true() -> bool {
    true
}

/// Per-tool display template overrides. Keys are the server-side tool name
/// (e.g. `"exec"`, `"read_file"`), not the prefixed display name.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ToolDisplayTemplates {
    #[serde(default)]
    pub call_template: Option<String>,
    #[serde(default)]
    pub result_template: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct McpServerConfig {
    #[serde(default)]
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub roots: Vec<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub rename_tools: HashMap<String, String>,
    /// Per-tool MiniJinja display templates. Overrides `_meta` templates from the MCP server.
    #[serde(default)]
    pub tool_templates: HashMap<String, ToolDisplayTemplates>,
    #[serde(default)]
    pub hooks: Option<HooksConfig>,
    /// The package this server belongs to, if it came from an installed package.
    /// Not serialized — set at runtime by the package loader.
    /// Used to present the server under its bare name when the active agent
    /// belongs to the same package, and under a prefixed name otherwise.
    #[serde(skip)]
    pub package: Option<String>,
}
