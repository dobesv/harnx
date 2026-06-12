//! `LlamaServerConfig` — per-provider config for the llama-server subprocess
//! provider. Manages a local `llama-server` (llama.cpp) child process listening
//! on a Unix domain socket, serving OpenAI-compatible chat completions.

use serde::{Deserialize, Serialize};

use crate::api_types::ExtraConfig;
use crate::model::{ModelData, RequestPatches};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LlamaServerConfig {
    pub name: Option<String>,

    #[serde(default)]
    pub models: Vec<ModelData>,

    pub patches: Option<RequestPatches>,

    pub extra: Option<ExtraConfig>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt_prefix: Option<Vec<String>>,

    /// Path to the GGUF model file (passed to llama-server via `-m`).
    pub model_path: String,

    /// Override path to the llama-server binary. If unset, the runtime
    /// searches PATH, then falls back to the `HARNX_LLAMA_SERVER_BIN` env var.
    pub binary_path: Option<String>,

    /// Context size in tokens (`-c` flag). Defaults to llama-server's default
    /// (typically 512) if unset.
    #[serde(default)]
    pub ctx_size: Option<u32>,

    /// Number of GPU layers to offload (`-ngl` flag). Zero means CPU-only.
    #[serde(default)]
    pub n_gpu_layers: Option<u32>,

    /// Number of threads (`-t` flag). Defaults to llama-server's default if unset.
    #[serde(default)]
    pub threads: Option<u32>,

    /// Raw passthrough arguments to llama-server.
    pub extra_args: Option<Vec<String>>,

    /// Override Unix socket path. If unset, runtime uses
    /// `~/.local/share/harnx/llama-server-<pid>-<hash>.sock`.
    pub socket_path: Option<String>,

    /// Runtime-only: the package this client was loaded from, if any.
    /// Not persisted to YAML (serde skip).
    #[serde(skip)]
    pub package: Option<String>,
}
