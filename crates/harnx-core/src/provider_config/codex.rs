//! `CodexConfig` — per-provider config for the ChatGPT/Codex subscription client.
//!
//! Authenticates with an existing ChatGPT Pro/Plus/Team subscription via the
//! credentials the official `codex` CLI writes to `~/.codex/auth.json`, instead
//! of a metered `OPENAI_API_KEY`. Requests go to OpenAI's Codex backend using
//! the Responses API.

use serde::{Deserialize, Serialize};

use crate::api_types::ExtraConfig;
use crate::model::{ModelData, RequestPatches};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CodexConfig {
    #[serde(skip)]
    pub name: String,

    /// Path to the Codex CLI `auth.json`. Defaults to `~/.codex/auth.json`
    /// when unset.
    pub auth_file: Option<String>,

    /// Optional API base override (advanced/testing). Defaults to the Codex
    /// backend base URL when unset.
    pub api_base: Option<String>,

    #[serde(default)]
    pub models: Vec<ModelData>,
    pub patches: Option<RequestPatches>,
    pub extra: Option<ExtraConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt_prefix: Option<Vec<String>>,

    /// Runtime-only: the package this client was loaded from, if any.
    /// Not persisted to YAML (serde skip).
    #[serde(skip)]
    pub package: Option<String>,
}
