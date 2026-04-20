//! `BedrockConfig` — per-provider config for AWS Bedrock.

use serde::Deserialize;

use crate::api_types::ExtraConfig;
use crate::model::{ModelData, RequestPatch};

#[derive(Debug, Clone, Deserialize)]
pub struct BedrockConfig {
    pub name: Option<String>,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
    pub region: Option<String>,
    pub session_token: Option<String>,
    #[serde(default)]
    pub models: Vec<ModelData>,
    pub patch: Option<RequestPatch>,
    pub extra: Option<ExtraConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt_prefix: Option<Vec<String>>,
}
