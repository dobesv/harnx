use super::vertexai::*;
use super::*;

use anyhow::{Context, Result, bail};
use reqwest::RequestBuilder;
use serde::Deserialize;
use serde_json::{json, Value};

const API_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";
const GEMINI_MAX_BATCH_SIZE: usize = 100;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct GeminiConfig {
    pub name: Option<String>,
    pub api_key: Option<String>,
    pub api_base: Option<String>,
    #[serde(default)]
    pub models: Vec<ModelData>,
    pub patch: Option<RequestPatch>,
    pub extra: Option<ExtraConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt_prefix: Option<Vec<String>>,
}

impl GeminiClient {
    config_get_fn!(api_key, get_api_key);
    config_get_fn!(api_base, get_api_base);

    pub const PROMPTS: [PromptAction<'static>; 1] = [("api_key", "API Key", None)];
}

impl_client_trait!(
    GeminiClient,
    (
        prepare_chat_completions,
        gemini_chat_completions,
        gemini_chat_completions_streaming
    ),
    (prepare_embeddings, embeddings),
    (noop_prepare_rerank, noop_rerank),
);

fn prepare_chat_completions(
    self_: &GeminiClient,
    data: ChatCompletionsData,
) -> Result<RequestData> {
    let api_key = self_.get_api_key()?;
    let api_base = self_
        .get_api_base()
        .unwrap_or_else(|_| API_BASE.to_string());

    let func = match data.stream {
        true => "streamGenerateContent",
        false => "generateContent",
    };

    let url = format!(
        "{}/models/{}:{}",
        api_base.trim_end_matches('/'),
        self_.model.real_name(),
        func
    );

    let body = gemini_build_chat_completions_body(data, &self_.model)?;

    let mut request_data = RequestData::new(url, body);

    request_data.header("x-goog-api-key", api_key);
    request_data.header("Content-Type", "application/json");
    request_data.header("Content-Type", "application/json");

    Ok(request_data)
}

fn prepare_embeddings(self_: &GeminiClient, data: &EmbeddingsData) -> Result<RequestData> {
    if data.texts.len() > GEMINI_MAX_BATCH_SIZE {
        bail!(
            "Gemini embeddings support at most {GEMINI_MAX_BATCH_SIZE} texts per request, got {}",
            data.texts.len()
        );
    }

    let api_key = self_.get_api_key()?;
    let api_base = self_
        .get_api_base()
        .unwrap_or_else(|_| API_BASE.to_string());

    let requests: Vec<Value> = data
        .texts
        .iter()
        .map(|text| {
            json!({
                "model": self_.model.real_name(),
                "content": {
                    "parts": [
                        {
                            "text": text
                        }
                    ]
                }
            })
        })
        .collect();

    let body = json!({
        "requests": requests,
    });

    let mut request_data = RequestData::new(
        format!(
            "{}/models/{}:batchEmbedContents",
            api_base.trim_end_matches('/'),
            self_.model.real_name(),
        ),
        body,
    );
    request_data.header("x-goog-api-key", api_key);
    request_data.header("Content-Type", "application/json");

    Ok(request_data)
}

async fn embeddings(builder: RequestBuilder, _model: &Model) -> Result<EmbeddingsOutput> {
    let res = builder.send().await?;
    let status = res.status();
    let data: Value = res.json().await?;
    if !status.is_success() {
        catch_error(&data, status.as_u16())?;
    }
    let res_body: EmbeddingsResBody =
        serde_json::from_value(data).context("Invalid embeddings data")?;
    let output = res_body
        .embeddings
        .into_iter()
        .map(|v| v.values)
        .collect();
    Ok(output)
}

#[derive(Deserialize)]
struct EmbeddingsResBody {
    embeddings: Vec<EmbeddingsResBodyEmbedding>,
}

#[derive(Deserialize)]
struct EmbeddingsResBodyEmbedding {
    values: Vec<f32>,
}
