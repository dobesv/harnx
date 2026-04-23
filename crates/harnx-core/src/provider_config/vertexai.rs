//! `VertexAIConfig` — per-provider config for Google Vertex AI.

use serde::Deserialize;

use crate::api_types::ExtraConfig;
use crate::model::{ModelData, RequestPatch};

#[derive(Debug, Clone, Deserialize, Default)]
pub struct VertexAIConfig {
    pub name: Option<String>,
    pub project_id: Option<String>,
    pub location: Option<String>,
    pub adc_file: Option<String>,
    #[serde(default)]
    pub models: Vec<ModelData>,
    pub patch: Option<RequestPatch>,
    pub extra: Option<ExtraConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt_prefix: Option<Vec<String>>,
}
