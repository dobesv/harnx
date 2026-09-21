//! `CohereConfig` — per-provider config for Cohere.

use serde::{Deserialize, Serialize};

use crate::api_types::ExtraConfig;
use crate::model::{ModelData, RequestPatches};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CohereConfig {
    #[serde(skip)]
    pub name: String,
    pub api_key: Option<String>,
    pub api_base: Option<String>,
    /// Catalog block in `models.yaml` to inherit models from, by provider
    /// name (e.g. `bedrock`). Omit it and the client's filename decides,
    /// which is the older behaviour kept for compatibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_catalog: Option<String>,
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
