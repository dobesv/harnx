use serde::{Deserialize, Serialize};
use std::collections::HashMap;

fn default_true() -> bool {
    true
}

fn default_idle_timeout() -> u64 {
    600
}

fn default_operation_timeout() -> u64 {
    3600
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct AcpServerConfig {
    #[serde(default)]
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub description: Option<String>,
    /// ACP subprocess idle backstop in seconds. LLM HTTP client read timeouts should
    /// fail stalled requests first; this only catches fully silent subprocess hangs.
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout_secs: u64,
    #[serde(default = "default_operation_timeout")]
    pub operation_timeout_secs: u64,
    /// The package this server belongs to, if it came from an installed package.
    /// Not serialized — set at runtime by the package loader.
    #[serde(skip)]
    pub package: Option<String>,
}
