use crate::codex_auth::CodexCreds;
use crate::*;

use anyhow::Result;

const CODEX_API_BASE: &str = "https://chatgpt.com/backend-api/codex";

impl CodexClient {
    // Read from env (`CODEX_API_BASE` / `CODEX_AUTH_FILE`, per the client's
    // filename stem) first, then the config file. Both are optional, so callers
    // use `.ok()` and fall back to protocol defaults.
    config_get_fn!(api_base, get_api_base);
    config_get_fn!(auth_file, get_auth_file);

    pub const PROMPTS: [PromptAction<'static>; 0] = [];
}

#[async_trait::async_trait]
impl Client for CodexClient {
    client_common_fns!();

    async fn chat_completions_inner(
        &self,
        client: &reqwest::Client,
        data: ChatCompletionsData,
    ) -> Result<ChatCompletionsOutput> {
        let auth_file = self.get_auth_file().ok();
        let creds =
            crate::codex_auth::prepare_codex_access_token(client, self.name(), &auth_file).await?;
        let model = codex_model_with_responses_endpoint(self.model());
        let api_base = self.get_api_base().ok();
        let request_data = build_codex_request(api_base.as_deref(), &model, &creds, data)?;
        let builder = self.request_builder(client, request_data)?;
        crate::openai::openai_chat_completions(builder, &model).await
    }

    async fn chat_completions_streaming_inner(
        &self,
        client: &reqwest::Client,
        handler: &mut SseHandler,
        data: ChatCompletionsData,
    ) -> Result<()> {
        let auth_file = self.get_auth_file().ok();
        let creds =
            crate::codex_auth::prepare_codex_access_token(client, self.name(), &auth_file).await?;
        let model = codex_model_with_responses_endpoint(self.model());
        let api_base = self.get_api_base().ok();
        let request_data = build_codex_request(api_base.as_deref(), &model, &creds, data)?;
        let builder = self.request_builder(client, request_data)?;
        crate::openai::openai_chat_completions_streaming(builder, handler, &model).await
    }
}

fn build_codex_request(
    config_api_base: Option<&str>,
    model: &Model,
    creds: &CodexCreds,
    data: ChatCompletionsData,
) -> Result<RequestData> {
    let api_base = config_api_base
        .unwrap_or(CODEX_API_BASE)
        .trim_end_matches('/');
    // The subscription bearer token is loaded from auth.json, not supplied per
    // request, so refuse to attach it to a plaintext or non-HTTPS base URL where
    // it could leak. HTTPS overrides (including local test servers) stay allowed.
    require_https_base(api_base)?;
    let url = format!("{api_base}/responses");
    let mut body = crate::openai_responses::openai_build_responses_body(data, model);
    body["store"] = serde_json::json!(false);
    let needs_instr = body
        .get("instructions")
        .and_then(|value| value.as_str())
        .map(|instructions| instructions.is_empty())
        .unwrap_or(true);
    if needs_instr {
        body["instructions"] = serde_json::json!("You are a helpful assistant.");
    }

    let mut request_data = RequestData::new(url, body);
    request_data.bearer_auth(&creds.access_token);
    if let Some(account_id) = &creds.account_id {
        request_data.header("chatgpt-account-id", account_id);
    }
    request_data.header("Accept", "text/event-stream");
    request_data.header("OpenAI-Beta", "responses=experimental");
    request_data.header("originator", "codex_cli_rs");
    Ok(request_data)
}

/// Reject any Codex base URL that isn't HTTPS. The scheme comparison is
/// case-insensitive per RFC 3986. This guards the auto-loaded subscription
/// token against plaintext (`http://`) or other non-TLS transports; the
/// `api_base` override is meant for pointing at alternate HTTPS backends.
fn require_https_base(api_base: &str) -> Result<()> {
    let scheme = api_base.split_once("://").map(|(scheme, _)| scheme);
    match scheme {
        Some(scheme) if scheme.eq_ignore_ascii_case("https") => Ok(()),
        _ => anyhow::bail!(
            "Codex api_base must use https:// to protect the subscription token; got `{api_base}`"
        ),
    }
}

fn codex_model_with_responses_endpoint(model: &Model) -> Model {
    let mut model = model.clone();
    if model.endpoint() != Some("responses") {
        model.data_mut().endpoint = Some("responses".to_string());
    }
    model
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::provider_config::codex::CodexConfig;

    fn chat_data() -> ChatCompletionsData {
        ChatCompletionsData {
            messages: vec![Message::new(
                MessageRole::User,
                MessageContent::Text("Hello".to_string()),
            )],
            temperature: None,
            top_p: None,
            functions: None,
            stream: false,
            attachments_dir: None,
        }
    }

    fn codex_client() -> CodexClient {
        let config = CodexConfig {
            name: "codex".to_string(),
            ..Default::default()
        };
        let model = Model::new("codex", "gpt-5-codex");
        CodexClient::from_config_for_test(config, model)
    }

    #[test]
    fn codex_request_uses_subscription_endpoint_headers_and_body() {
        let client = codex_client();
        let model = codex_model_with_responses_endpoint(client.model());
        let creds = CodexCreds {
            access_token: "tok".to_string(),
            account_id: Some("acc_1".to_string()),
        };

        let request_data = build_codex_request(None, &model, &creds, chat_data())
            .expect("Codex request should build");

        assert!(request_data.url.ends_with("/responses"));
        assert!(request_data
            .url
            .starts_with("https://chatgpt.com/backend-api/codex"));
        assert_eq!(
            request_data.headers.get("authorization").map(String::as_str),
            Some("Bearer tok")
        );
        assert_eq!(
            request_data
                .headers
                .get("chatgpt-account-id")
                .map(String::as_str),
            Some("acc_1")
        );
        assert_eq!(
            request_data.headers.get("Accept").map(String::as_str),
            Some("text/event-stream")
        );
        assert_eq!(
            request_data.headers.get("OpenAI-Beta").map(String::as_str),
            Some("responses=experimental")
        );
        assert_eq!(
            request_data.headers.get("originator").map(String::as_str),
            Some("codex_cli_rs")
        );
        assert_eq!(request_data.body["store"], serde_json::json!(false));
        assert!(request_data.body["instructions"]
            .as_str()
            .is_some_and(|instructions| !instructions.is_empty()));
    }

    #[test]
    fn codex_request_rejects_http_base_without_leaking_credentials() {
        let model = codex_model_with_responses_endpoint(&Model::new("codex", "gpt-5-codex"));
        // Use a distinctive secret so the leak assertion can't collide with
        // ordinary words like "token" in the error message.
        let secret = "sk-secret-value-12345";
        let creds = CodexCreds {
            access_token: secret.to_string(),
            account_id: Some("acc_1".to_string()),
        };

        // An http:// override must be refused before any request (and therefore
        // any bearer token) is constructed. Match rather than `expect_err` since
        // the Ok variant (`RequestData`) isn't `Debug`.
        let message = match build_codex_request(
            Some("http://evil.example"),
            &model,
            &creds,
            chat_data(),
        ) {
            Ok(_) => panic!("http api_base must be rejected"),
            Err(err) => err.to_string(),
        };
        assert!(
            message.contains("https://"),
            "error should explain the requirement: {message}"
        );
        assert!(
            !message.contains(secret),
            "error must not leak the access token: {message}"
        );
    }

    #[test]
    fn codex_request_rejects_non_https_scheme() {
        let model = codex_model_with_responses_endpoint(&Model::new("codex", "gpt-5-codex"));
        let creds = CodexCreds {
            access_token: "tok".to_string(),
            account_id: None,
        };

        for base in ["ftp://host/x", "chatgpt.com/backend-api/codex", "HTTP://host"] {
            assert!(
                build_codex_request(Some(base), &model, &creds, chat_data()).is_err(),
                "non-https base `{base}` must be rejected"
            );
        }
    }

    #[test]
    fn codex_request_allows_https_override() {
        let model = codex_model_with_responses_endpoint(&Model::new("codex", "gpt-5-codex"));
        let creds = CodexCreds {
            access_token: "tok".to_string(),
            account_id: None,
        };

        // Case-insensitive scheme; a custom HTTPS backend (e.g. a test server)
        // is allowed and routed to the /responses path.
        let request_data =
            build_codex_request(Some("HTTPS://proxy.example/base"), &model, &creds, chat_data())
                .expect("https override should build");
        assert_eq!(request_data.url, "HTTPS://proxy.example/base/responses");
    }

    #[test]
    fn codex_model_forces_responses_endpoint() {
        let model = Model::new("codex", "gpt-5-codex");
        assert_eq!(model.endpoint(), None);

        let model = codex_model_with_responses_endpoint(&model);

        assert_eq!(model.endpoint(), Some("responses"));
    }

    #[test]
    fn codex_is_registered_client_type() {
        assert!(crate::list_client_types().contains(&"codex"));
    }
}
