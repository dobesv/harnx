//! `LlamaServerConfig` — per-provider config for the llama-server subprocess
//! provider. Manages a local `llama-server` (llama.cpp) child process listening
//! on a Unix domain socket, serving OpenAI-compatible chat completions.
//!
//! Each model in `models[]` specifies its own GGUF path and tuning knobs,
//! allowing one config to serve multiple local models. Selecting a model
//! selects its corresponding subprocess (lazy spawn, reused, kill-on-drop).

use serde::{Deserialize, Serialize};

use crate::api_types::ExtraConfig;
use crate::model::{ModelData, RequestPatches};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LlamaServerConfig {
    pub name: Option<String>,

    /// Models served by this provider. Each model specifies its own
    /// GGUF path and tuning knobs; selecting a model spawns its subprocess.
    #[serde(default)]
    pub models: Vec<ModelData>,

    pub patches: Option<RequestPatches>,

    pub extra: Option<ExtraConfig>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt_prefix: Option<Vec<String>>,

    /// Override path to the llama-server binary. If unset, the runtime
    /// searches PATH, then falls back to the `HARNX_LLAMA_SERVER_BIN` env var.
    /// Shared across all models in this config (discovery is the same binary).
    pub binary_path: Option<String>,

    /// Runtime-only: the package this client was loaded from, if any.
    /// Not persisted to YAML (serde skip).
    #[serde(skip)]
    pub package: Option<String>,
}
