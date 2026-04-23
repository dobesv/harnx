use serde::{Deserialize, Serialize};
use std::collections::HashMap;

fn default_true() -> bool {
    true
}

fn default_idle_timeout() -> u64 {
    300
}

fn default_operation_timeout() -> u64 {
    3600
}

#[derive(Debug, Clone, Deserialize, Serialize)]
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
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout_secs: u64,
    #[serde(default = "default_operation_timeout")]
    pub operation_timeout_secs: u64,
}
