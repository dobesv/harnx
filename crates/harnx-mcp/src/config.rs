use serde::{Deserialize, Serialize};
use std::collections::HashMap;

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Serialize)]
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
}
