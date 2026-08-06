use crate::{OpenAICompatibleClient, SseHandler, OPENAI_COMPATIBLE_PROVIDERS};

use anyhow::{bail, Context, Result};
use indexmap::IndexMap;
use parking_lot::RwLock;
use reqwest::{Client as ReqwestClient, RequestBuilder};
use serde_json::Value;
use std::sync::LazyLock;
use std::time::Duration;

const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 10;
const DEFAULT_READ_TIMEOUT_SECS: u64 = 120;

#[allow(unused_imports)]
pub use harnx_core::api_types::{
    ChatCompletionsData, ChatCompletionsOutput, CompletionTokenUsage, EmbeddingsData,
    EmbeddingsOutput, ExtraConfig, RerankData, RerankOutput, RerankResult,
};
pub use harnx_core::error::LlmError;
pub use harnx_core::message::{
    extract_system_message, ImageUrl, Message, MessageContent, MessageContentPart,
    MessageContentToolCalls, MessageRole,
};
pub use harnx_core::model::{Model, ModelData, ModelType, ProviderModels, RequestPatches};
pub use harnx_core::tool::ToolCall;

/// Parse retry/cooldown duration from HTTP response headers.
///
/// Checks `Retry-After` (seconds or HTTP-date), `x-ratelimit-reset-requests`,
/// and `x-ratelimit-reset-tokens`, returning the maximum duration found.
pub fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let mut max_duration: Option<Duration> = None;

    let mut consider = |d: Duration| {
        max_duration = Some(match max_duration {
            Some(current) => current.max(d),
            None => d,
        });
    };

    // Standard Retry-After header (seconds integer or HTTP-date)
    if let Some(val) = headers.get("retry-after").and_then(|v| v.to_str().ok()) {
        if let Ok(secs) = val.parse::<u64>() {
            consider(Duration::from_secs(secs));
        } else if let Some(d) = safe_duration_from_secs_f64(val.parse::<f64>().ok()) {
            consider(d);
        } else if let Some(d) = parse_http_date_retry_after(val) {
            consider(d);
        }
    }

    // OpenAI-style rate limit reset headers (values in seconds or duration strings like "1s", "2m")
    for header_name in ["x-ratelimit-reset-requests", "x-ratelimit-reset-tokens"] {
        if let Some(val) = headers.get(header_name).and_then(|v| v.to_str().ok()) {
            if let Some(d) = parse_duration_value(val) {
                consider(d);
            }
        }
    }

    max_duration
}

/// Convert an `Option<f64>` to a `Duration`, returning `None` for negative, NaN, or infinite values.
fn safe_duration_from_secs_f64(val: Option<f64>) -> Option<Duration> {
    let v = val?;
    if v.is_finite() && v >= 0.0 {
        Some(Duration::from_secs_f64(v))
    } else {
        None
    }
}

/// Parse an RFC 2616 / RFC 7231 HTTP-date `Retry-After` value into a duration from now.
fn parse_http_date_retry_after(val: &str) -> Option<Duration> {
    use chrono::{DateTime, Utc};
    // Try common HTTP date formats: RFC 2822, RFC 850, asctime
    let target = DateTime::parse_from_rfc2822(val)
        .map(|dt| dt.with_timezone(&Utc))
        .or_else(|_| {
            DateTime::parse_from_str(val, "%A, %d-%b-%y %T GMT").map(|dt| dt.with_timezone(&Utc))
        })
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(val, "%a %b %e %T %Y").map(|ndt| ndt.and_utc())
        })
        .ok()?;
    let now = Utc::now();
    if target > now {
        let diff = target - now;
        diff.to_std().ok()
    } else {
        Some(Duration::ZERO)
    }
}

/// Parse a duration value that may be seconds (integer/float) or a simple duration string like "1s", "2m", "500ms".
fn parse_duration_value(val: &str) -> Option<Duration> {
    let val = val.trim();
    if let Some(d) = safe_duration_from_secs_f64(val.parse::<f64>().ok()) {
        return Some(d);
    }
    if let Some(s) = val.strip_suffix("ms") {
        let ms = s.trim().parse::<f64>().ok()?;
        return if ms.is_finite() && ms >= 0.0 {
            Some(Duration::from_secs_f64(ms / 1000.0))
        } else {
            None
        };
    }
    if let Some(s) = val.strip_suffix('s') {
        return safe_duration_from_secs_f64(s.trim().parse::<f64>().ok());
    }
    if let Some(s) = val.strip_suffix('m') {
        let mins = s.trim().parse::<f64>().ok()?;
        return if mins.is_finite() && mins >= 0.0 {
            Some(Duration::from_secs_f64(mins * 60.0))
        } else {
            None
        };
    }
    None
}

const MODELS_YAML: &str = include_str!("../../harnx/models.yaml");

/// Optional override list installed by the host (harnx) at startup.
/// When set, `ALL_PROVIDER_MODELS` uses this instead of the embedded
/// `models.yaml` on first access. Must be installed before any client
/// initialization triggers `ALL_PROVIDER_MODELS` evaluation.
static MODELS_OVERRIDE: RwLock<Option<Vec<ProviderModels>>> = RwLock::new(None);

/// Install a list of provider models to override the default list
/// parsed from the embedded `models.yaml`. Call this once at startup
/// before any client initialization.
pub fn install_models_override(models: Vec<ProviderModels>) {
    *MODELS_OVERRIDE.write() = Some(models);
}

pub static ALL_PROVIDER_MODELS: LazyLock<Vec<ProviderModels>> = LazyLock::new(|| {
    if let Some(models) = MODELS_OVERRIDE.read().clone() {
        return models;
    }
    serde_yaml::from_str(MODELS_YAML).unwrap()
});

/// Per-call configuration values that a `Client` implementation needs
/// to read during a single `chat_completions` or `embeddings` call.
///
/// Populated by the caller from `GlobalConfig` before each call so that
/// provider clients don't need to hold a reference to `GlobalConfig`.
/// That independence is what eventually lets the client layer live in
/// its own crate.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClientCallContext<'a> {
    /// Optional `User-Agent` header to send on HTTP requests. Pulled
    /// from `GlobalConfig.user_agent`.
    pub user_agent: Option<&'a str>,
    /// When true, the client short-circuits network calls and returns
    /// a stub response. Pulled from `GlobalConfig.dry_run`.
    pub dry_run: bool,
}

fn set_proxy(mut builder: reqwest::ClientBuilder, proxy: &str) -> Result<reqwest::ClientBuilder> {
    builder = builder.no_proxy();
    if !proxy.is_empty() && proxy != "-" {
        builder = builder
            .proxy(reqwest::Proxy::all(proxy).with_context(|| format!("Invalid proxy `{proxy}`"))?);
    };
    Ok(builder)
}

fn resolve_timeout_secs(configured: Option<u64>, default: u64) -> u64 {
    // reqwest treats 0 as infinite timeout; coerce it back to default so protections stay on.
    configured.filter(|&secs| secs > 0).unwrap_or(default)
}

#[async_trait::async_trait]
pub trait Client: Sync + Send {
    fn extra_config(&self) -> Option<&ExtraConfig>;

    fn patches_config(&self) -> Option<&RequestPatches>;

    fn name(&self) -> &str;

    fn model(&self) -> &Model;

    fn model_mut(&mut self) -> &mut Model;

    /// Returns true if this client expands `cid:` attachment references internally
    /// during request build (e.g., via File API upload), rather than relying on
    /// the runtime base64 pre-pass.
    ///
    /// When true:
    /// - Runtime skips the base64 expansion pre-pass for this client
    /// - Raw `cid:` references are preserved in message ImageUrl parts
    /// - `ChatCompletionsData.attachments_dir` is set so the client can read blobs
    ///
    /// When false (default):
    /// - Runtime base64 pre-pass expands all `cid:` refs before the client sees them
    /// - Client receives `data:` URLs which it handles according to its protocol
    ///
    /// Note: Implementations must be callable through the trait object, so inherent
    /// methods with the same signature won't be used unless the trait default is
    /// explicitly overridden in an `impl Client for ...` block.
    fn expands_attachments_internally(&self) -> bool {
        false
    }

    fn build_client(&self, ctx: &ClientCallContext<'_>) -> Result<ReqwestClient> {
        let mut builder = ReqwestClient::builder();
        let extra = self.extra_config();
        let connect_timeout = resolve_timeout_secs(
            extra.and_then(|v| v.connect_timeout),
            DEFAULT_CONNECT_TIMEOUT_SECS,
        );
        let read_timeout = resolve_timeout_secs(
            extra.and_then(|v| v.read_timeout),
            DEFAULT_READ_TIMEOUT_SECS,
        );
        if let Some(proxy) = extra.and_then(|v| v.proxy.as_deref()) {
            builder = set_proxy(builder, proxy)?;
        }
        if let Some(user_agent) = ctx.user_agent {
            builder = builder.user_agent(user_agent);
        }
        if let Some(true) = extra.and_then(|v| v.accept_invalid_certs) {
            builder = builder.danger_accept_invalid_certs(true);
        }
        if let Some(ca_cert) = extra.and_then(|v| v.ca_cert.as_deref()) {
            let cert_data = std::fs::read(ca_cert)
                .with_context(|| format!("Failed to read CA certificate from '{ca_cert}'"))?;
            let cert = reqwest::Certificate::from_pem(&cert_data)
                .with_context(|| format!("Invalid CA certificate in '{ca_cert}'"))?;
            builder = builder.add_root_certificate(cert);
        }
        if let Some(client_cert) = extra.and_then(|v| v.client_cert.as_deref()) {
            let mut identity_data = std::fs::read(client_cert).with_context(|| {
                format!("Failed to read client certificate from '{client_cert}'")
            })?;
            if let Some(client_key) = extra.and_then(|v| v.client_key.as_deref()) {
                let key_data = std::fs::read(client_key)
                    .with_context(|| format!("Failed to read client key from '{client_key}'"))?;
                identity_data.push(b'\n');
                identity_data.extend_from_slice(&key_data);
            }
            let identity = reqwest::Identity::from_pem(&identity_data)
                .with_context(|| format!("Invalid client certificate/key from '{client_cert}'. If the cert and key are in separate files, ensure 'client_key' is also set."))?;
            builder = builder.identity(identity);
        } else if extra.and_then(|v| v.client_key.as_deref()).is_some() {
            warn!("'client_key' is set but 'client_cert' is missing; mTLS identity will not be configured");
        }
        let client = builder
            .connect_timeout(Duration::from_secs(connect_timeout))
            .read_timeout(Duration::from_secs(read_timeout))
            .build()
            .with_context(|| "Failed to build client")?;
        Ok(client)
    }

    async fn embeddings(
        &self,
        data: &EmbeddingsData,
        ctx: &ClientCallContext<'_>,
    ) -> Result<Vec<Vec<f32>>> {
        let client = self.build_client(ctx)?;
        self.embeddings_inner(&client, data)
            .await
            .context("Failed to call embeddings api")
    }

    async fn rerank(&self, data: &RerankData, ctx: &ClientCallContext<'_>) -> Result<RerankOutput> {
        let client = self.build_client(ctx)?;
        self.rerank_inner(&client, data)
            .await
            .context("Failed to call rerank api")
    }

    async fn chat_completions_inner(
        &self,
        client: &ReqwestClient,
        data: ChatCompletionsData,
    ) -> Result<ChatCompletionsOutput>;

    async fn chat_completions_streaming_inner(
        &self,
        client: &ReqwestClient,
        handler: &mut SseHandler,
        data: ChatCompletionsData,
    ) -> Result<()>;

    async fn embeddings_inner(
        &self,
        _client: &ReqwestClient,
        _data: &EmbeddingsData,
    ) -> Result<EmbeddingsOutput> {
        bail!("The client doesn't support embeddings api")
    }

    async fn rerank_inner(
        &self,
        _client: &ReqwestClient,
        _data: &RerankData,
    ) -> Result<RerankOutput> {
        bail!("The client doesn't support rerank api")
    }

    fn request_builder(
        &self,
        client: &reqwest::Client,
        mut request_data: RequestData,
    ) -> Result<RequestBuilder> {
        self.patch_request_data(&mut request_data)?;
        Ok(request_data.into_builder(client))
    }

    fn patch_request_data(&self, request_data: &mut RequestData) -> Result<()> {
        let mut json_value = request_data.to_json_value();

        if let Some(patches) = self.model().patches() {
            let source = format!("model `{}`", self.model().id());
            json_value = apply_request_patches(&source, patches, json_value)?;
        }

        if let Some(patches_config) = self.patches_config() {
            if let Some(patches) = self
                .model()
                .model_type()
                .extract_patches_for(patches_config, self.model().endpoint())
            {
                let source = format!("client `{}`", self.name());
                json_value = apply_request_patches(&source, patches, json_value)?;
            }
        }

        let api_name = match (self.model().model_type(), self.model().endpoint()) {
            (ModelType::Chat, Some("responses")) => "RESPONSES",
            (ModelType::Chat, _) => "CHAT_COMPLETIONS",
            (ModelType::Embedding, _) => "EMBEDDINGS",
            (ModelType::Reranker, _) => "RERANK",
        };
        let env_name = format!("HARNX_PATCH_{}_{}", self.name(), api_name).to_ascii_uppercase();
        if let Ok(raw_patches) = std::env::var(&env_name) {
            let patches: Vec<String> = serde_json::from_str(&raw_patches)
                .with_context(|| format!("Invalid JSON patch array in {env_name}"))?;
            json_value = apply_request_patches(&env_name, &patches, json_value)?;
        }

        *request_data = RequestData::from_json_value(json_value)?;
        Ok(())
    }
}

/// Applies one group of request patches, failing the request if any of them
/// errors. Skipping a broken patch would send a body the patch was meant to
/// correct (an unsupported `temperature`, a missing thinking config) and the
/// only trace used to be a `warn!` in the debug log.
fn apply_request_patches(source: &str, patches: &[String], json_value: Value) -> Result<Value> {
    harnx_core::jaq::eval_filters_strict(patches, json_value)
        .with_context(|| format!("Failed to apply request patches from {source}"))
}

impl Default for crate::ClientConfig {
    fn default() -> Self {
        Self::OpenAIConfig(harnx_core::provider_config::openai::OpenAIConfig::default())
    }
}

pub struct RequestData {
    pub url: String,
    pub headers: IndexMap<String, String>,
    pub body: Value,
}

impl RequestData {
    pub fn new<T>(url: T, body: Value) -> Self
    where
        T: std::fmt::Display,
    {
        Self {
            url: url.to_string(),
            headers: Default::default(),
            body,
        }
    }

    pub fn bearer_auth<T>(&mut self, auth: T)
    where
        T: std::fmt::Display,
    {
        self.headers
            .insert("authorization".into(), format!("Bearer {auth}"));
    }

    pub fn header<K, V>(&mut self, key: K, value: V)
    where
        K: std::fmt::Display,
        V: std::fmt::Display,
    {
        self.headers.insert(key.to_string(), value.to_string());
    }

    pub fn to_json_value(&self) -> serde_json::Value {
        serde_json::json!({
            "url": self.url,
            "headers": self.headers,
            "body": self.body,
        })
    }

    pub fn from_json_value(v: serde_json::Value) -> anyhow::Result<Self> {
        let mut obj = v
            .as_object()
            .cloned()
            .context("Patched request data must be a JSON object")?;

        let url = obj
            .remove("url")
            .context("Patched request data missing 'url'")?
            .as_str()
            .map(ToOwned::to_owned)
            .context("Patched request data field 'url' must be a string")?;

        let headers_value = obj
            .remove("headers")
            .context("Patched request data missing 'headers'")?;
        let headers: IndexMap<String, String> = serde_json::from_value(headers_value)
            .context("Patched request data field 'headers' must be an object of strings")?;

        let body = obj
            .remove("body")
            .context("Patched request data missing 'body'")?;

        Ok(Self { url, headers, body })
    }

    pub fn into_builder(self, client: &ReqwestClient) -> RequestBuilder {
        let RequestData { url, headers, body } = self;
        debug!("Request {url} {body}");
        harnx_core::llm_trace::request(&url, &body);

        let mut builder = client.post(url);
        for (key, value) in headers {
            builder = builder.header(key, value);
        }
        builder = builder.json(&body);
        builder
    }
}

pub type PromptAction<'a> = (&'a str, &'a str, Option<&'a str>);

/// Returns the default API base URL for a named OpenAI-compatible provider,
/// or `None` if the provider is not a known preset.
pub fn openai_compatible_api_base(client: &str) -> Option<&'static str> {
    OPENAI_COMPATIBLE_PROVIDERS
        .into_iter()
        .find(|(name, _)| client == *name)
        .map(|(_, api_base)| api_base)
}

/// Returns true when `client` is the literal name of the
/// OpenAI-compatible catch-all provider.
pub fn is_openai_compatible_provider_name(client: &str) -> bool {
    client == OpenAICompatibleClient::NAME
}

pub fn noop_prepare_embeddings<T>(_client: &T, _data: &EmbeddingsData) -> Result<RequestData> {
    bail!("The client doesn't support embeddings api")
}

pub async fn noop_embeddings(_builder: RequestBuilder, _model: &Model) -> Result<EmbeddingsOutput> {
    bail!("The client doesn't support embeddings api")
}

pub fn noop_prepare_rerank<T>(_client: &T, _data: &RerankData) -> Result<RequestData> {
    bail!("The client doesn't support rerank api")
}

pub async fn noop_rerank(_builder: RequestBuilder, _model: &Model) -> Result<RerankOutput> {
    bail!("The client doesn't support rerank api")
}

pub fn catch_error(data: &Value, status: u16, retry_after: Option<Duration>) -> Result<()> {
    if (200..300).contains(&status) {
        return Ok(());
    }
    debug!("Invalid response, status: {status}, data: {data}");
    let message = if let Some(error) = data["error"].as_object() {
        if let (Some(typ), Some(message)) = (
            json_str_from_map(error, "type"),
            json_str_from_map(error, "message"),
        ) {
            format!("{message} (type: {typ})")
        } else if let (Some(typ), Some(message)) = (
            json_str_from_map(error, "code"),
            json_str_from_map(error, "message"),
        ) {
            format!("{message} (code: {typ})")
        } else {
            format!("Invalid response data: {data} (status: {status})")
        }
    } else if let Some(error) = data["errors"][0].as_object() {
        if let (Some(code), Some(message)) = (
            error.get("code").and_then(|v| v.as_u64()),
            json_str_from_map(error, "message"),
        ) {
            format!("{message} (status: {code})")
        } else {
            format!("Invalid response data: {data} (status: {status})")
        }
    } else if let Some(error) = data[0]["error"].as_object() {
        if let (Some(err_status), Some(message)) = (
            json_str_from_map(error, "status"),
            json_str_from_map(error, "message"),
        ) {
            format!("{message} (status: {err_status})")
        } else {
            format!("Invalid response data: {data} (status: {status})")
        }
    } else if let (Some(detail), Some(code)) = (data["detail"].as_str(), data["status"].as_i64()) {
        format!("{detail} (status: {code})")
    } else if let Some(error) = data["error"].as_str() {
        error.to_string()
    } else if let Some(message) = data["message"].as_str() {
        message.to_string()
    } else {
        format!("Invalid response data: {data} (status: {status})")
    };
    Err(LlmError {
        status,
        message,
        retry_after,
    }
    .into())
}

pub fn json_str_from_map<'a>(
    map: &'a serde_json::Map<String, Value>,
    field_name: &str,
) -> Option<&'a str> {
    map.get(field_name).and_then(|v| v.as_str())
}

#[cfg(test)]
mod request_data_tests {
    use super::RequestData;
    use harnx_core::provider_config::openai::OpenAIConfig;
    use indexmap::IndexMap;
    use serde_json::json;

    #[test]
    fn request_data_to_json_value_round_trips() {
        let mut headers = IndexMap::new();
        headers.insert("authorization".to_string(), "Bearer test-token".to_string());
        headers.insert("content-type".to_string(), "application/json".to_string());

        let request_data = RequestData {
            url: "https://api.example.com/v1/chat/completions".to_string(),
            headers,
            body: json!({
                "model": "gpt-4o",
                "messages": [{"role": "user", "content": "Hello"}]
            }),
        };

        let json_value = request_data.to_json_value();

        // Verify structure matches expected {url, headers, body}
        assert_eq!(
            json_value.get("url").and_then(|v| v.as_str()),
            Some("https://api.example.com/v1/chat/completions")
        );
        let headers_obj = json_value
            .get("headers")
            .and_then(|v| v.as_object())
            .unwrap();
        assert_eq!(
            headers_obj.get("authorization").and_then(|v| v.as_str()),
            Some("Bearer test-token")
        );
        assert_eq!(
            headers_obj.get("content-type").and_then(|v| v.as_str()),
            Some("application/json")
        );
        let body = json_value.get("body").unwrap();
        assert_eq!(body.get("model").and_then(|v| v.as_str()), Some("gpt-4o"));
    }

    #[test]
    fn request_data_from_json_value_restores_fields() {
        let json_value = json!({
            "url": "https://api.example.com/v1/embeddings",
            "headers": {
                "authorization": "Bearer embed-token",
                "x-custom-header": "custom-value"
            },
            "body": {
                "model": "text-embedding-3-small",
                "input": "test input"
            }
        });

        let request_data = RequestData::from_json_value(json_value).expect("should parse JSON");

        assert_eq!(request_data.url, "https://api.example.com/v1/embeddings");
        assert_eq!(
            request_data.headers.get("authorization"),
            Some(&"Bearer embed-token".to_string())
        );
        assert_eq!(
            request_data.headers.get("x-custom-header"),
            Some(&"custom-value".to_string())
        );
        assert_eq!(
            request_data.body.get("model").and_then(|v| v.as_str()),
            Some("text-embedding-3-small")
        );
        assert_eq!(
            request_data.body.get("input").and_then(|v| v.as_str()),
            Some("test input")
        );
    }

    #[test]
    fn patch_request_data_applies_jaq_filters_via_model_patches() {
        // Create a RequestData with initial values
        let mut headers = IndexMap::new();
        headers.insert("authorization".to_string(), "Bearer original".to_string());

        let request_data = RequestData {
            url: "https://api.original.com/v1/chat".to_string(),
            headers,
            body: json!({
                "model": "original-model",
                "messages": [{"role": "user", "content": "Hello"}]
            }),
        };

        // Convert to JSON, apply a jaq filter, and convert back
        let mut json_value = request_data.to_json_value();

        // Simulate what patch_request_data does: apply jaq filters
        let patches = vec![
            r#".url = "https://api.patched.com/v1/chat""#.to_string(),
            r#".headers.authorization = "Bearer patched-token""#.to_string(),
            r#".body.model = "patched-model""#.to_string(),
        ];
        json_value = super::apply_request_patches("model `openai:test`", &patches, json_value)
            .expect("valid patches should apply");

        let patched_data =
            RequestData::from_json_value(json_value).expect("should parse patched JSON");

        // Verify the patches were applied
        assert_eq!(patched_data.url, "https://api.patched.com/v1/chat");
        assert_eq!(
            patched_data.headers.get("authorization"),
            Some(&"Bearer patched-token".to_string())
        );
        assert_eq!(
            patched_data.body.get("model").and_then(|v| v.as_str()),
            Some("patched-model")
        );
        // Original content should still be present
        assert_eq!(
            patched_data
                .body
                .get("messages")
                .and_then(|v| v.as_array())
                .map(|a| a.len()),
            Some(1)
        );
    }

    #[test]
    fn env_var_patch_format_json_array_deserializes_and_applies() {
        // Simulates HARNX_PATCH_OPENAI_CHAT_COMPLETIONS env var value
        let env_var_value = r#"[".body.max_tokens = 100", ".body.temperature = 0.5"]"#;
        let patches: Vec<String> = serde_json::from_str(env_var_value)
            .expect("env var should parse as JSON array of strings");

        let input = serde_json::json!({"url": "...", "headers": {}, "body": {"model": "gpt-4o"}});
        let output =
            super::apply_request_patches("HARNX_PATCH_OPENAI_CHAT_COMPLETIONS", &patches, input)
                .expect("valid patches should apply");

        assert_eq!(output["body"]["max_tokens"].as_i64(), Some(100));
        assert_eq!(output["body"]["temperature"].as_f64(), Some(0.5));
    }

    #[test]
    fn apply_request_patches_reports_the_failing_patch_and_reason() {
        // `.body.reasoning.effort` needs a `.body.reasoning` object; jaq won't
        // create one. The request must fail loudly rather than go out unpatched.
        let patches = vec![r#".body.reasoning.effort = "high""#.to_string()];
        let input = serde_json::json!({"url": "...", "headers": {}, "body": {"model": "gpt-5.6"}});

        let err = super::apply_request_patches("model `openai:gpt-5.6`", &patches, input)
            .expect_err("assigning through a missing object must fail");
        let message = format!("{err:#}");

        assert!(
            message.contains("model `openai:gpt-5.6`"),
            "error should name the patch source: {message}"
        );
        assert!(
            message.contains(".body.reasoning.effort"),
            "error should name the failing expression: {message}"
        );
        assert!(
            message.contains("cannot use null as iterable"),
            "error should carry the jaq reason: {message}"
        );
    }

    #[test]
    fn extra_config_deserializes_read_timeout() {
        let yaml = r#"
type: openai
extra:
  connect_timeout: 7
  read_timeout: 45
"#;

        let config: OpenAIConfig = serde_yaml::from_str(yaml).expect("parse OpenAI config");
        let extra = config.extra.expect("extra config");

        assert_eq!(extra.connect_timeout, Some(7));
        assert_eq!(extra.read_timeout, Some(45));
    }

    #[test]
    fn resolve_timeout_secs_coerces_zero_to_default() {
        assert_eq!(
            super::resolve_timeout_secs(Some(0), super::DEFAULT_CONNECT_TIMEOUT_SECS),
            super::DEFAULT_CONNECT_TIMEOUT_SECS
        );
        assert_eq!(
            super::resolve_timeout_secs(Some(0), super::DEFAULT_READ_TIMEOUT_SECS),
            super::DEFAULT_READ_TIMEOUT_SECS
        );
        assert_eq!(
            super::resolve_timeout_secs(Some(7), super::DEFAULT_CONNECT_TIMEOUT_SECS),
            7
        );
        assert_eq!(
            super::resolve_timeout_secs(None, super::DEFAULT_READ_TIMEOUT_SECS),
            super::DEFAULT_READ_TIMEOUT_SECS
        );
    }
}
