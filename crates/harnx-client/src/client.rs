use crate::{OpenAICompatibleClient, SseHandler, OPENAI_COMPATIBLE_PROVIDERS};

use anyhow::{bail, Context, Result};
use fancy_regex::Regex;
use indexmap::IndexMap;
use parking_lot::RwLock;
use reqwest::{Client as ReqwestClient, RequestBuilder};
use serde_json::Value;
use std::sync::LazyLock;
use std::time::Duration;

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
pub use harnx_core::model::{Model, ModelData, ModelType, ProviderModels, RequestPatch};
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

static ESCAPE_SLASH_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?<!\\)/").unwrap());

static PATCH_VAR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\$\{(\w+)\}|\$([A-Z_][A-Z0-9_]*)").unwrap());

/// Interpolates `$VAR` and `${VAR}` patterns in header values.
///
/// Built-in variables:
/// - `HARNX_MODEL` → model_name
/// - `HARNX_CLIENT` → client_name
///
/// Falls back to environment variables for other names.
/// Returns an error if a variable cannot be resolved.
pub fn interpolate_patch_vars(value: &str, model_name: &str, client_name: &str) -> Result<String> {
    let result = PATCH_VAR_RE.replace_all(value, |caps: &fancy_regex::Captures| {
        let var_name = caps.get(1).or_else(|| caps.get(2)).unwrap().as_str();

        match var_name {
            "HARNX_MODEL" => model_name.to_string(),
            "HARNX_CLIENT" => client_name.to_string(),
            _ => {
                match std::env::var(var_name) {
                    Ok(val) => val,
                    Err(_) => {
                        // Return a marker that we'll detect after the replace_all
                        format!("__UNRESOLVED__{var_name}__")
                    }
                }
            }
        }
    });

    // Check if there are any unresolved variables
    if result.contains("__UNRESOLVED__") {
        // Extract the variable name from the marker
        if let Some(start) = result.find("__UNRESOLVED__") {
            let marker_start = start + "__UNRESOLVED__".len();
            if let Some(marker_end) = result[marker_start..].find("__") {
                let var_name = &result[marker_start..marker_start + marker_end];
                bail!("Unresolved variable '${var_name}' in patch header.\n  Built-in variables: $HARNX_MODEL, $HARNX_CLIENT\n  Other names are resolved from environment variables.\n  To fix: use a built-in, or set the env var: export {var_name}=<value>");
            }
        }
    }

    Ok(result.into_owned())
}

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

fn patch_env_name(client_name: &str, api_name: &str) -> String {
    // Mirrors harnx's `get_env_name("patch_{client}_{api}")` — hard-coded
    // to the `HARNX_` prefix so that moving this code into `harnx-client`
    // preserves behavior regardless of `CARGO_CRATE_NAME`.
    format!("HARNX_PATCH_{}_{}", client_name, api_name).to_ascii_uppercase()
}

#[async_trait::async_trait]
pub trait Client: Sync + Send {
    fn extra_config(&self) -> Option<&ExtraConfig>;

    fn patch_config(&self) -> Option<&RequestPatch>;

    fn name(&self) -> &str;

    fn model(&self) -> &Model;

    fn model_mut(&mut self) -> &mut Model;

    fn build_client(&self, ctx: &ClientCallContext<'_>) -> Result<ReqwestClient> {
        let mut builder = ReqwestClient::builder();
        let extra = self.extra_config();
        let timeout = extra.and_then(|v| v.connect_timeout).unwrap_or(10);
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
            .connect_timeout(Duration::from_secs(timeout))
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
        let model_type = self.model().model_type();
        if let Some(patch) = self.model().patch() {
            request_data.apply_patch(patch.clone());
            // Interpolate variables in headers after applying model-level patch
            for value in request_data.headers.values_mut() {
                *value =
                    interpolate_patch_vars(value, self.model().name(), self.model().client_name())?;
            }
        }

        let patch_map = std::env::var(patch_env_name(
            self.model().client_name(),
            model_type.api_name(),
        ))
        .ok()
        .and_then(|v| serde_json::from_str(&v).ok())
        .or_else(|| {
            self.patch_config()
                .and_then(|v| model_type.extract_patch(v))
                .cloned()
        });
        let patch_map = match patch_map {
            Some(v) => v,
            _ => return Ok(()),
        };
        for (key, patch) in patch_map {
            let key = ESCAPE_SLASH_RE.replace_all(&key, r"\/");
            if let Ok(regex) = Regex::new(&format!("^({key})$")) {
                if let Ok(true) = regex.is_match(self.model().name()) {
                    request_data.apply_patch(patch);
                    // Interpolate variables in headers after applying config-level patch
                    for value in request_data.headers.values_mut() {
                        *value = interpolate_patch_vars(
                            value,
                            self.model().name(),
                            self.model().client_name(),
                        )?;
                    }
                    return Ok(());
                }
            }
        }
        Ok(())
    }
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

    pub fn into_builder(self, client: &ReqwestClient) -> RequestBuilder {
        let RequestData { url, headers, body } = self;
        debug!("Request {url} {body}");

        let mut builder = client.post(url);
        for (key, value) in headers {
            builder = builder.header(key, value);
        }
        builder = builder.json(&body);
        builder
    }

    pub fn apply_patch(&mut self, patch: Value) {
        if let Some(patch_url) = patch["url"].as_str() {
            self.url = patch_url.into();
        }
        if let Some(patch_body) = patch.get("body") {
            json_patch::merge(&mut self.body, patch_body)
        }
        if let Some(patch_headers) = patch["headers"].as_object() {
            for (key, value) in patch_headers {
                if let Some(value) = value.as_str() {
                    self.header(key, value)
                } else if value.is_null() {
                    self.headers.swap_remove(key);
                }
            }
        }
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
mod tests {
    use super::*;

    #[test]
    fn test_interpolate_harnx_model() {
        let result = interpolate_patch_vars("$HARNX_MODEL", "gpt-4", "openai");
        assert_eq!(result.unwrap(), "gpt-4");
    }

    #[test]
    fn test_interpolate_harnx_client() {
        let result = interpolate_patch_vars("${HARNX_CLIENT}", "gpt-4", "openai");
        assert_eq!(result.unwrap(), "openai");
    }

    #[test]
    fn test_interpolate_multiple_vars() {
        let result = interpolate_patch_vars(
            "cc=$HARNX_MODEL; app=$HARNX_CLIENT",
            "claude-3",
            "anthropic",
        );
        assert_eq!(result.unwrap(), "cc=claude-3; app=anthropic");
    }

    #[test]
    fn test_interpolate_no_vars() {
        let result = interpolate_patch_vars("no variables here", "gpt-4", "openai");
        assert_eq!(result.unwrap(), "no variables here");
    }

    #[test]
    fn test_interpolate_unknown_var_error() {
        let result = interpolate_patch_vars("$UNKNOWN_VAR", "gpt-4", "openai");
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Unresolved variable '$UNKNOWN_VAR'"));
        assert!(err_msg.contains("$HARNX_MODEL, $HARNX_CLIENT"));
        assert!(err_msg.contains("export UNKNOWN_VAR="));
    }

    #[test]
    fn test_interpolate_env_var() {
        unsafe {
            std::env::set_var("TEST_VAR_PATCH", "test_value");
        }
        let result = interpolate_patch_vars("$TEST_VAR_PATCH", "gpt-4", "openai");
        assert_eq!(result.unwrap(), "test_value");
        unsafe {
            std::env::remove_var("TEST_VAR_PATCH");
        }
    }

    #[test]
    fn test_interpolate_empty_string() {
        let result = interpolate_patch_vars("", "gpt-4", "openai");
        assert_eq!(result.unwrap(), "");
    }

    #[test]
    fn test_interpolate_mixed_vars_and_text() {
        unsafe {
            std::env::set_var("CUSTOM_HEADER", "custom_value");
        }
        let result =
            interpolate_patch_vars("Bearer $HARNX_MODEL-$CUSTOM_HEADER", "gpt-4", "openai");
        assert_eq!(result.unwrap(), "Bearer gpt-4-custom_value");
        unsafe {
            std::env::remove_var("CUSTOM_HEADER");
        }
    }

    #[test]
    fn test_interpolate_braced_and_unbraced() {
        let result = interpolate_patch_vars("${HARNX_MODEL}:$HARNX_CLIENT", "gpt-4", "openai");
        assert_eq!(result.unwrap(), "gpt-4:openai");
    }
}
