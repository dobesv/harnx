use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::Utc;
use parking_lot::RwLock;
use serde::Deserialize;
use serde_json::Value;

use crate::{get_access_token, is_valid_access_token, set_access_token};

const AUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const TOKEN_EXP_SKEW_SECS: i64 = 60;

static REFRESH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static REFRESH_TOKENS: LazyLock<RwLock<HashMap<String, RefreshToken>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

#[derive(Clone)]
struct RefreshToken(String);

#[derive(Debug, PartialEq)]
enum TokenDecision {
    UseCached,
    ReuseFileToken { token: String, expires_at: i64 },
    Refresh,
}

/// The access token stored in `auth.json`, paired with its decoded expiry.
/// Bundled so the token and its lifetime travel together through the token
/// decision instead of as loose arguments.
struct FileToken {
    access_token: String,
    exp: Option<i64>,
}

/// Current time and the freshness margin used when deciding whether a stored
/// token is still safe to reuse.
struct TokenClock {
    now: i64,
    skew_secs: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AuthDotJson {
    #[serde(default, rename = "OPENAI_API_KEY")]
    pub openai_api_key: Option<String>,
    #[serde(default)]
    pub tokens: Option<TokenData>,
    #[serde(default)]
    pub last_refresh: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TokenData {
    #[serde(default)]
    pub id_token: String,
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub account_id: Option<String>,
}

pub struct CodexCreds {
    pub access_token: String,
    pub account_id: Option<String>,
}

pub fn default_auth_file() -> PathBuf {
    dirs::home_dir().map_or_else(
        || PathBuf::from(".codex/auth.json"),
        |home| home.join(".codex/auth.json"),
    )
}

fn resolve_auth_path(auth_file: &Option<String>) -> PathBuf {
    let Some(path) = auth_file else {
        return default_auth_file();
    };

    if path == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(path));
    }
    if let Some(relative_path) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(relative_path);
        }
    }

    PathBuf::from(path)
}

pub fn read_auth(path: &Path) -> Result<AuthDotJson> {
    let data = match std::fs::read_to_string(path) {
        Ok(data) => data,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            bail!(
                "Codex auth file not found at {}; install the codex CLI and run `codex login` first",
                path.display()
            )
        }
        Err(err) => {
            return Err(err)
                .with_context(|| format!("Failed to read Codex auth file at {}", path.display()))
        }
    };

    serde_json::from_str(&data)
        .with_context(|| format!("Failed to parse Codex auth file at {}", path.display()))
}

pub fn decode_jwt_claims(jwt: &str) -> Result<Value> {
    let mut segments = jwt.split('.');
    let (Some(header), Some(payload), Some(signature), None) = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) else {
        bail!("Malformed JWT: expected three dot-separated segments")
    };
    if [header, payload, signature].iter().any(|s| s.is_empty()) {
        bail!("Malformed JWT: expected three non-empty segments")
    }

    let payload = URL_SAFE_NO_PAD
        .decode(payload)
        .context("Malformed JWT: payload is not valid base64url")?;
    serde_json::from_slice(&payload).context("Malformed JWT: payload is not valid JSON")
}

pub fn account_id_from_tokens(tokens: &TokenData) -> Option<String> {
    if let Some(account_id) = tokens.account_id.as_ref().filter(|id| !id.is_empty()) {
        return Some(account_id.clone());
    }

    let claims = decode_jwt_claims(&tokens.id_token).ok()?;
    claims
        .get("https://api.openai.com/auth")
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .or_else(|| claims.get("chatgpt_account_id").and_then(Value::as_str))
        .map(str::to_owned)
}

pub fn access_token_exp(access_token: &str) -> Option<i64> {
    decode_jwt_claims(access_token).ok()?.get("exp")?.as_i64()
}

fn decide_token_action(cache_valid: bool, file: FileToken, clock: TokenClock) -> TokenDecision {
    if cache_valid {
        return TokenDecision::UseCached;
    }
    match file.exp {
        Some(expires_at)
            if !file.access_token.is_empty() && expires_at > clock.now + clock.skew_secs =>
        {
            TokenDecision::ReuseFileToken {
                token: file.access_token,
                expires_at,
            }
        }
        _ => TokenDecision::Refresh,
    }
}

fn remember_refresh_token(client_name: &str, refresh_token: RefreshToken) {
    REFRESH_TOKENS
        .write()
        .insert(client_name.to_owned(), refresh_token);
}

fn current_refresh_token(client_name: &str, file_refresh_token: RefreshToken) -> RefreshToken {
    REFRESH_TOKENS
        .read()
        .get(client_name)
        .cloned()
        .unwrap_or(file_refresh_token)
}

async fn refresh_token(
    client: &reqwest::Client,
    refresh_token: &RefreshToken,
) -> Result<(String, Option<RefreshToken>, i64)> {
    if refresh_token.0.is_empty() {
        bail!("Codex auth file has no refresh token; run `codex login`")
    }

    let form = format!(
        "grant_type={}&client_id={}&refresh_token={}",
        urlencoding::encode("refresh_token"),
        urlencoding::encode(CLIENT_ID),
        urlencoding::encode(&refresh_token.0)
    );
    let response = client
        .post(AUTH_TOKEN_URL)
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(form)
        .send()
        .await
        .context("Failed to refresh Codex access token")?;
    let status = response.status();
    if !status.is_success() {
        bail!("Codex token refresh failed with status {status}")
    }
    let body = response
        .text()
        .await
        .context("Failed to read Codex token response")?;
    let value: Value = serde_json::from_str(&body)
        .context("Invalid Codex token response (could not parse JSON)")?;
    let access_token = value
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| anyhow!("Codex token response missing access_token"))?
        .to_owned();
    let new_refresh_token = value
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .map(|token| RefreshToken(token.to_owned()));
    let expires_at = access_token_exp(&access_token).unwrap_or_else(|| {
        Utc::now().timestamp().saturating_add(
            value
                .get("expires_in")
                .and_then(Value::as_i64)
                .unwrap_or(3000),
        )
    });

    Ok((access_token, new_refresh_token, expires_at))
}

pub async fn prepare_codex_access_token(
    client: &reqwest::Client,
    client_name: &str,
    auth_file: &Option<String>,
) -> Result<CodexCreds> {
    let auth_path = resolve_auth_path(auth_file);
    let auth = read_auth(&auth_path)?;
    let tokens = auth
        .tokens
        .ok_or_else(|| anyhow!("Codex auth file has no tokens; run `codex login`"))?;
    let account_id = account_id_from_tokens(&tokens);

    let cache_valid = is_valid_access_token(client_name);
    let access_token = if cache_valid {
        get_access_token(client_name)?
    } else {
        let _refresh_guard = REFRESH_LOCK.lock().await;
        if is_valid_access_token(client_name) {
            get_access_token(client_name)?
        } else {
            let file = FileToken {
                access_token: tokens.access_token.clone(),
                exp: access_token_exp(&tokens.access_token),
            };
            let clock = TokenClock {
                now: Utc::now().timestamp(),
                skew_secs: TOKEN_EXP_SKEW_SECS,
            };
            match decide_token_action(false, file, clock) {
                TokenDecision::UseCached => get_access_token(client_name)?,
                TokenDecision::ReuseFileToken { token, expires_at } => {
                    set_access_token(client_name, token.clone(), expires_at);
                    token
                }
                TokenDecision::Refresh => {
                    let refresh_token_value =
                        current_refresh_token(client_name, RefreshToken(tokens.refresh_token));
                    let (access_token, new_refresh_token, expires_at) =
                        refresh_token(client, &refresh_token_value).await?;
                    if let Some(new_refresh_token) = new_refresh_token {
                        remember_refresh_token(client_name, new_refresh_token);
                    }
                    // v1: in-memory token cache only; we deliberately do NOT write refreshed
                    // tokens back to auth.json to avoid races with official codex CLI. Rotated
                    // refresh tokens are held in memory too. TODO: optional opt-in write-back
                    // with an OS file lock.
                    set_access_token(client_name, access_token.clone(), expires_at);
                    access_token
                }
            }
        }
    };

    Ok(CodexCreds {
        access_token,
        account_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(value: &str) -> String {
        URL_SAFE_NO_PAD.encode(value)
    }

    fn unsigned_jwt(payload_json: &str) -> String {
        format!("{}.{}.{}", b64("{}"), b64(payload_json), "sig")
    }

    fn token_data() -> TokenData {
        TokenData {
            id_token: String::new(),
            access_token: String::new(),
            refresh_token: String::new(),
            account_id: None,
        }
    }

    fn file_token(access_token: &str, exp: Option<i64>) -> FileToken {
        FileToken {
            access_token: access_token.to_string(),
            exp,
        }
    }

    fn clock_at(now: i64) -> TokenClock {
        TokenClock { now, skew_secs: 60 }
    }

    #[test]
    fn decide_token_action_uses_cached_token() {
        assert_eq!(
            decide_token_action(true, file_token("file-token", Some(4_600)), clock_at(1_000)),
            TokenDecision::UseCached
        );
    }

    #[test]
    fn decide_token_action_refreshes_for_unusable_file_tokens() {
        assert_eq!(
            decide_token_action(false, file_token("", Some(4_600)), clock_at(1_000)),
            TokenDecision::Refresh
        );
        assert_eq!(
            decide_token_action(
                false,
                file_token("file-token", Some(1_030)),
                clock_at(1_000)
            ),
            TokenDecision::Refresh
        );
        assert_eq!(
            decide_token_action(false, file_token("file-token", None), clock_at(1_000)),
            TokenDecision::Refresh
        );
    }

    #[test]
    fn decide_token_action_reuses_valid_file_token() {
        assert_eq!(
            decide_token_action(
                false,
                file_token("file-token", Some(4_600)),
                clock_at(1_000)
            ),
            TokenDecision::ReuseFileToken {
                token: "file-token".to_string(),
                expires_at: 4_600,
            }
        );
    }

    #[test]
    fn read_auth_parses_auth_json() {
        let temp = tempfile::NamedTempFile::new().expect("create auth file");
        std::fs::write(
            temp.path(),
            r#"{
                "OPENAI_API_KEY": null,
                "tokens": {
                    "access_token": "access.jwt.value",
                    "refresh_token": "refresh-token",
                    "id_token": "id.jwt.value",
                    "account_id": "acc_123"
                },
                "last_refresh": "2026-01-28T08:05:37Z"
            }"#,
        )
        .expect("write auth file");

        let auth = read_auth(temp.path()).expect("read auth file");
        let tokens = auth.tokens.expect("tokens");
        assert_eq!(tokens.access_token, "access.jwt.value");
        assert_eq!(tokens.refresh_token, "refresh-token");
        assert_eq!(tokens.account_id.as_deref(), Some("acc_123"));
    }

    #[test]
    fn read_auth_not_found_mentions_login() {
        let temp = tempfile::tempdir().expect("create temp directory");
        let path = temp.path().join("missing-auth.json");

        let error = read_auth(&path).expect_err("missing auth file should fail");
        let message = error.to_string();
        assert!(message.contains("not found"));
        assert!(message.contains("codex login"));
    }

    #[test]
    fn read_auth_malformed_json_mentions_path() {
        let temp = tempfile::NamedTempFile::new().expect("create auth file");
        std::fs::write(temp.path(), "not json").expect("write malformed auth file");

        let error = read_auth(temp.path()).expect_err("malformed auth file should fail");
        let message = error.to_string();
        assert!(message.contains("Failed to parse Codex auth file"));
        assert!(message.contains(&temp.path().display().to_string()));
    }

    #[test]
    fn default_auth_file_uses_codex_auth_suffix() {
        assert!(default_auth_file().ends_with(Path::new(".codex/auth.json")));
    }

    #[test]
    fn resolve_auth_path_expands_home_and_preserves_other_paths() {
        let tilde_path = resolve_auth_path(&Some("~/.codex/auth.json".to_string()));
        if let Some(home) = dirs::home_dir() {
            assert!(tilde_path.starts_with(home));
        } else {
            assert_eq!(tilde_path, PathBuf::from("~/.codex/auth.json"));
        }
        assert!(tilde_path.ends_with(Path::new(".codex/auth.json")));

        assert_eq!(
            resolve_auth_path(&Some("/abs/x.json".to_string())),
            PathBuf::from("/abs/x.json")
        );
        assert_eq!(resolve_auth_path(&None), default_auth_file());
    }

    #[test]
    fn rotated_refresh_token_overrides_file_token() {
        let client_name = "codex-auth-rotated-refresh-test";
        REFRESH_TOKENS.write().remove(client_name);
        assert_eq!(
            current_refresh_token(client_name, RefreshToken("file-token".to_string())).0,
            "file-token"
        );

        remember_refresh_token(client_name, RefreshToken("rotated-token".to_string()));

        assert_eq!(
            current_refresh_token(client_name, RefreshToken("file-token".to_string())).0,
            "rotated-token"
        );
        REFRESH_TOKENS.write().remove(client_name);
    }

    #[tokio::test]
    async fn refresh_token_rejects_empty_token_before_request() {
        let result = refresh_token(&reqwest::Client::new(), &RefreshToken(String::new())).await;
        let Err(error) = result else {
            panic!("empty refresh token should fail");
        };

        assert!(error.to_string().contains("refresh token"));
    }

    #[test]
    fn decode_jwt_claims_reads_payload() {
        let jwt = unsigned_jwt(r#"{"sub":"user_123","active":true}"#);

        let claims = decode_jwt_claims(&jwt).expect("decode claims");

        assert_eq!(claims["sub"], "user_123");
        assert_eq!(claims["active"], true);
    }

    #[test]
    fn decode_jwt_claims_rejects_non_jwt() {
        let err = decode_jwt_claims("not-a-jwt").expect_err("reject malformed JWT");

        assert!(err.to_string().contains("Malformed JWT"));
    }

    #[test]
    fn account_id_prefers_token_data_value() {
        let tokens = TokenData {
            account_id: Some("acc_explicit".to_string()),
            id_token: unsigned_jwt(
                r#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acc_claim"}}"#,
            ),
            ..token_data()
        };

        assert_eq!(
            account_id_from_tokens(&tokens).as_deref(),
            Some("acc_explicit")
        );
    }

    #[test]
    fn account_id_reads_id_token_claim() {
        let tokens = TokenData {
            id_token: unsigned_jwt(
                r#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acc_123"}}"#,
            ),
            ..token_data()
        };

        assert_eq!(account_id_from_tokens(&tokens).as_deref(), Some("acc_123"));
    }

    #[test]
    fn access_token_exp_reads_exp_claim() {
        let access_token = unsigned_jwt(r#"{"exp":9999999999}"#);

        assert_eq!(access_token_exp(&access_token), Some(9_999_999_999));
    }

    #[test]
    fn auth_dot_json_deserializes_full_file() {
        let auth: AuthDotJson = serde_json::from_str(
            r#"{
                "auth_mode": "chatgpt",
                "OPENAI_API_KEY": null,
                "tokens": {
                    "access_token": "access.jwt.value",
                    "refresh_token": "refresh-token",
                    "id_token": "id.jwt.value",
                    "account_id": "acc_123"
                },
                "last_refresh": "2026-01-28T08:05:37Z"
            }"#,
        )
        .expect("deserialize auth file");

        assert_eq!(auth.openai_api_key, None);
        let tokens = auth.tokens.expect("tokens");
        assert_eq!(tokens.access_token, "access.jwt.value");
        assert_eq!(tokens.refresh_token, "refresh-token");
        assert_eq!(tokens.id_token, "id.jwt.value");
        assert_eq!(tokens.account_id.as_deref(), Some("acc_123"));
        assert_eq!(auth.last_refresh.as_deref(), Some("2026-01-28T08:05:37Z"));
    }
}
