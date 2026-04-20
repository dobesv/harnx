//! `AzureOpenAIConfig` — per-provider config for Azure-hosted OpenAI.

use serde::Deserialize;

use crate::api_types::ExtraConfig;
use crate::model::{ModelData, RequestPatch};

#[derive(Debug, Clone, Deserialize)]
pub struct AzureOpenAIConfig {
    pub name: Option<String>,
    pub api_base: Option<String>,
    pub api_key: Option<String>,
    #[serde(default)]
    pub models: Vec<ModelData>,
    pub patch: Option<RequestPatch>,
    pub extra: Option<ExtraConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt_prefix: Option<Vec<String>>,
}
