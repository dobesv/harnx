use std::fmt;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Duration, Utc};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

pub const DEFAULT_GITHUB_API_URL: &str = "https://api.github.com";
pub const GITHUB_ACCEPT: &str = "application/vnd.github+json";
pub const GITHUB_API_VERSION: &str = "2022-11-28";
pub const USER_AGENT: &str = "harnx-mcp-plans-github/1";
pub const JWT_LIFETIME_SECONDS: i64 = 10 * 60;
pub const TOKEN_REFRESH_SKEW_SECONDS: i64 = 5 * 60;

pub trait Clock: Send + Sync + fmt::Debug {
    fn now(&self) -> DateTime<Utc>;
}

#[derive(Debug, Clone, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoConfig {
    pub owner: String,
    pub repo: String,
}

impl RepoConfig {
    pub fn parse(value: &str) -> Result<Self> {
        let trimmed = value.trim();
        let (owner, repo) = trimmed
            .split_once('/')
            .ok_or_else(|| anyhow!("repo must be owner/repo"))?;
        if owner.is_empty() || repo.is_empty() {
            bail!("repo must be owner/repo");
        }
        Ok(Self {
            owner: owner.to_owned(),
            repo: repo.to_owned(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppAuthConfig {
    pub app_id: String,
    pub private_key_pem: String,
    pub installation_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthSource {
    PersonalAccessToken(String),
    GitHubApp(AppAuthConfig),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthConfig {
    pub base_url: String,
    pub repo: RepoConfig,
    pub source: AuthSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenResponse {
    pub token: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct GitHubAuth {
    inner: Arc<GitHubAuthInner>,
}

#[derive(Debug)]
struct GitHubAuthInner {
    base_url: String,
    source: AuthSource,
    clock: Arc<dyn Clock>,
    http: Client,
    cached_installation_token: Mutex<Option<TokenResponse>>,
}

impl GitHubAuth {
    pub fn new(config: AuthConfig) -> Result<Self> {
        Self::with_clock(config, Arc::new(SystemClock))
    }

    pub fn with_clock(config: AuthConfig, clock: Arc<dyn Clock>) -> Result<Self> {
        let http = Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(StdDuration::from_secs(10))
            .timeout(StdDuration::from_secs(30))
            .build()
            .context("build GitHub auth HTTP client")?;
        Ok(Self {
            inner: Arc::new(GitHubAuthInner {
                base_url: normalize_base_url(&config.base_url)?,
                source: config.source,
                clock,
                http,
                cached_installation_token: Mutex::new(None),
            }),
        })
    }

    pub fn base_url(&self) -> &str {
        &self.inner.base_url
    }

    pub async fn bearer_token(&self) -> Result<String> {
        match &self.inner.source {
            AuthSource::PersonalAccessToken(token) => Ok(token.clone()),
            AuthSource::GitHubApp(app) => self.installation_token(app).await,
        }
    }

    pub async fn authorization_header_value(&self) -> Result<String> {
        Ok(format!("Bearer {}", self.bearer_token().await?))
    }

    async fn installation_token(&self, app: &AppAuthConfig) -> Result<String> {
        let now = self.inner.clock.now();
        {
            let cache = self.inner.cached_installation_token.lock().await;
            if let Some(cached) = cache.as_ref() {
                if now < cached.expires_at - Duration::seconds(TOKEN_REFRESH_SKEW_SECONDS) {
                    return Ok(cached.token.clone());
                }
            }
        }

        let fresh = self.exchange_installation_token(app).await?;
        let token = fresh.token.clone();
        *self.inner.cached_installation_token.lock().await = Some(fresh);
        Ok(token)
    }

    async fn exchange_installation_token(&self, app: &AppAuthConfig) -> Result<TokenResponse> {
        let jwt = self.mint_app_jwt(app)?;
        let url = format!(
            "{}/app/installations/{}/access_tokens",
            self.inner.base_url, app.installation_id
        );
        let response = self
            .inner
            .http
            .post(url)
            .header("Accept", GITHUB_ACCEPT)
            .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
            .bearer_auth(jwt)
            .send()
            .await
            .context("exchange GitHub App installation token")?
            .error_for_status()
            .context("GitHub App installation token exchange failed")?;
        let payload: InstallationTokenResponse = response
            .json()
            .await
            .context("decode GitHub installation token response")?;
        let expires_at = payload
            .expires_at
            .parse::<DateTime<Utc>>()
            .context("parse GitHub installation token expiry")?;
        Ok(TokenResponse {
            token: payload.token,
            expires_at,
        })
    }

    pub fn mint_app_jwt(&self, app: &AppAuthConfig) -> Result<String> {
        mint_app_jwt(&app.app_id, &app.private_key_pem, self.inner.clock.now())
    }
}

#[derive(Debug, Deserialize)]
struct InstallationTokenResponse {
    token: String,
    expires_at: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct AppClaims {
    iss: String,
    iat: i64,
    exp: i64,
}

pub fn mint_app_jwt(app_id: &str, private_key_pem: &str, now: DateTime<Utc>) -> Result<String> {
    let claims = AppClaims {
        iss: app_id.to_owned(),
        iat: now.timestamp(),
        exp: (now + Duration::seconds(JWT_LIFETIME_SECONDS)).timestamp(),
    };
    let mut header = Header::new(Algorithm::RS256);
    header.typ = Some("JWT".to_owned());
    let encoding_key =
        EncodingKey::from_rsa_pem(private_key_pem.as_bytes()).context("load RSA private key")?;
    jsonwebtoken::encode(&header, &claims, &encoding_key).context("encode GitHub App JWT")
}

pub fn load_private_key(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.contains("BEGIN") {
        return Ok(trimmed.to_owned());
    }
    let path = Path::new(trimmed);
    std::fs::read_to_string(path)
        .with_context(|| format!("read GitHub App private key from {}", path.display()))
}

fn normalize_base_url(value: &str) -> Result<String> {
    let trimmed = value.trim().trim_end_matches('/');
    let parsed = reqwest::Url::parse(trimmed).context("parse GITHUB_API_URL")?;
    match parsed.scheme() {
        "http" | "https" => Ok(trimmed.to_owned()),
        other => bail!("unsupported GitHub API URL scheme: {other}"),
    }
}

pub trait TokenProvider: Send + Sync {
    fn exchange<'a>(
        &'a self,
        jwt: &'a str,
        installation_id: &'a str,
        base_url: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<TokenResponse>> + Send + 'a>>;
}

impl TokenProvider for GitHubAuth {
    fn exchange<'a>(
        &'a self,
        _jwt: &'a str,
        _installation_id: &'a str,
        _base_url: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<TokenResponse>> + Send + 'a>> {
        Box::pin(async move {
            match &self.inner.source {
                AuthSource::GitHubApp(app) => self.exchange_installation_token(app).await,
                AuthSource::PersonalAccessToken(_) => {
                    bail!("PAT auth does not exchange installation tokens")
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use serde_json::Value;
    use std::collections::VecDeque;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const TEST_PRIVATE_KEY: &str = "-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQCzHH3Wumt4Pzxw
SHUzJiXg9lHE9i221HvJowEUYJG5qr5tBYbuwT64RF5g7COBWH8IDIXshm15zb9f
Vvmti7KrOYVXNGM/vKVZ1VT9u9Hc+5fyG3S8BCoj9zvj++qe4KhHPkhDEZokTfqV
XneYW5QjbmHRglLSizUJZO9Q6Dl3w5Dp2JXNfRTGHLvFlg1ZCs6+0kt0wrAJmurP
0tdwaYgW4Aiiwr3nH8ySSmdHFztSpRj2V5bhnCrqFrSzNrmr3bhxKGDnV6MRWPpw
LeV6UP6u2bueKOZpV1kSbZm7CyyCmNSyfv/cESoOaEkcc7OmBplFNeB1hCtLdbp6
U1rMifa3AgMBAAECggEAEWes4M9nvx0iAeVAolJMLwKwqnujsJcQWmVBJxpFPu4V
KCH33T6hGiXmp/N6dcjEO2OAJh5gCAyS0rBwfclL+PCTgQhhtqFdzg95fVihiaBX
QRsi6lxbPfh59OsXfw3kvFuUiRPuTyXWumoeJAuOJy1ESygnZTdK1Zld2hZe80C/
UnJ746m0Fi5TgLVEawSw0alKZlvSdkqbt1rKeBxYD7g+kWzjIabnYYFQs5V6un7G
xSkkJWdkmWAXGfUEAk+8Fxf1oLlmNocAOmmRosHNyCt8lrGBdCA9lRypzwvplmz8
bHUEK9YnHdIrB0LY8rHDAzDri2HkH7kD/SY2LD+pTQKBgQD1yQi67Kz91MkGWpAJ
jiIMhDowwhtmaUyg3tLqhw9H3Zocbi3Bd8hM4A3wuc+45lM6OCOSed/4ZD+RCrmA
lYZuGaHfYFlfM+Or9h+VnDF42MBZxItIyn9Kybb6qYRJA6+HOjvZJyOybItOem0+
CpeU3QHfukr34nSu/hiuyyBNcwKBgQC6jhj7QGtxu/Kfm/5karD4e1k2WRVv2Ytz
FdxgpSTxDS8UJ72VaAEXm4lflTYNEQfBMzkCbweMUGPFu+Zm63XN9OC9waQLJht0
PwNvwGY1isw7bwwiP3yjM/D4wvOJvY6FlpgQYfyF9r/m5guEE6MLhiIV0rOaX1S1
S2F4UAvgrQKBgQCV54Xmk+EycywkLun4mfKUVbUz9b9GZ+SvnRdgqP0d5L9QpbZM
cCT/FgwKjRlu+TM7p++yL5j1YxcN/E+FaCz0S7fZiGcZ1IkAYX2D/x6BSRmP5nrY
64BVec+a8/bVnWTaAh9sYx23fdI9DBhCpa0rwtuYu4NryndGH32oZgUOlwKBgGa1
qUdbdkxOAAykI/FBVGHZ94oWjdjg2wfnt0d2ZNpaOdtM7fH+KuvGdGGtku6qu6xA
+Vg/rNYxxFyvUPDFHjzgX4PZwulod6EOuGOkeCFuY3ctcm7AqWxpQniTTOY++OLP
wLT0XcWbzpffe+OhtBi6JrYBJWUOq2KNOAK3f3KZAoGABBEbkPTeWmikC6RAcKNX
4JtSjejYs6P9Zu/Q08YiTzY6r4QQK0eAOMpF6aptreyjF97CWzXAwbTqRusu/Py6
G1SjQmEsAspvcP0/1YPJ3ZU0PbmHFYqgElLkrvBMMT+9QoODBmzWrStN4FHQMQVS
se1WmgkjgdgmMWBVaajht3Y=
-----END PRIVATE KEY-----";

    #[derive(Debug)]
    struct FixedClock {
        now: StdMutex<DateTime<Utc>>,
    }

    impl FixedClock {
        fn new(now: DateTime<Utc>) -> Self {
            Self {
                now: StdMutex::new(now),
            }
        }

        fn set(&self, now: DateTime<Utc>) {
            *self.now.lock().expect("clock poisoned") = now;
        }
    }

    impl Clock for FixedClock {
        fn now(&self) -> DateTime<Utc> {
            *self.now.lock().expect("clock poisoned")
        }
    }

    #[test]
    fn minted_jwt_has_rs256_header_and_expected_claims() {
        let now = DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
            .expect("valid timestamp")
            .with_timezone(&Utc);
        let token = mint_app_jwt("12345", TEST_PRIVATE_KEY, now).expect("mint jwt");
        let parts: Vec<_> = token.split('.').collect();
        assert_eq!(parts.len(), 3);

        let header_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(parts[0])
            .expect("decode header");
        let claims_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(parts[1])
            .expect("decode claims");
        let header: Value = serde_json::from_slice(&header_bytes).expect("header json");
        let claims: Value = serde_json::from_slice(&claims_bytes).expect("claims json");

        assert_eq!(header["alg"], "RS256");
        assert_eq!(header["typ"], "JWT");
        assert_eq!(claims["iss"], "12345");
        assert_eq!(claims["iat"], now.timestamp());
        assert_eq!(claims["exp"], now.timestamp() + JWT_LIFETIME_SECONDS);
    }

    #[tokio::test]
    async fn cached_installation_token_refreshes_near_expiry() {
        let start = DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
            .expect("valid timestamp")
            .with_timezone(&Utc);
        let clock = Arc::new(FixedClock::new(start));
        let hits = Arc::new(AtomicUsize::new(0));
        let (base_url, server) = spawn_token_server(hits.clone()).await;
        let auth = GitHubAuth::with_clock(
            AuthConfig {
                base_url,
                repo: RepoConfig::parse("acme/plans").expect("repo"),
                source: AuthSource::GitHubApp(AppAuthConfig {
                    app_id: "1".to_owned(),
                    private_key_pem: TEST_PRIVATE_KEY.to_owned(),
                    installation_id: "2".to_owned(),
                }),
            },
            clock.clone(),
        )
        .expect("auth");

        let first = auth.bearer_token().await.expect("first token");
        let second = auth.bearer_token().await.expect("cached token");
        assert_eq!(first, "token-1");
        assert_eq!(second, "token-1");
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        clock.set(start + Duration::minutes(56));
        let third = auth.bearer_token().await.expect("refreshed token");
        assert_eq!(third, "token-2");
        assert_eq!(hits.load(Ordering::SeqCst), 2);

        server.abort();
    }

    #[tokio::test]
    async fn base_url_override_is_used_for_installation_exchange() {
        let hits = Arc::new(AtomicUsize::new(0));
        let (base_url, server) = spawn_token_server(hits.clone()).await;
        let clock = Arc::new(FixedClock::new(
            DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
                .expect("valid timestamp")
                .with_timezone(&Utc),
        ));
        let auth = GitHubAuth::with_clock(
            AuthConfig {
                base_url: base_url.clone(),
                repo: RepoConfig::parse("acme/plans").expect("repo"),
                source: AuthSource::GitHubApp(AppAuthConfig {
                    app_id: "1".to_owned(),
                    private_key_pem: TEST_PRIVATE_KEY.to_owned(),
                    installation_id: "99".to_owned(),
                }),
            },
            clock,
        )
        .expect("auth");

        let token = auth.bearer_token().await.expect("token");
        assert_eq!(token, "token-1");
        assert_eq!(auth.base_url(), base_url);
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        server.abort();
    }

    async fn spawn_token_server(hits: Arc<AtomicUsize>) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr: SocketAddr = listener.local_addr().expect("local addr");
        let responses = Arc::new(StdMutex::new(VecDeque::from([
            serde_json::json!({ "token": "token-1", "expires_at": "2026-01-02T04:04:05Z" })
                .to_string(),
            serde_json::json!({ "token": "token-2", "expires_at": "2026-01-02T05:10:05Z" })
                .to_string(),
        ])));
        let handle = tokio::spawn(async move {
            loop {
                let (mut stream, _) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => break,
                };
                hits.fetch_add(1, Ordering::SeqCst);
                let responses = responses.clone();
                tokio::spawn(async move {
                    let mut buf = [0_u8; 4096];
                    let _ = stream.read(&mut buf).await;
                    let payload = responses
                        .lock()
                        .expect("responses poisoned")
                        .pop_front()
                        .unwrap_or_else(|| {
                            serde_json::json!({
                                "token": "token-x",
                                "expires_at": "2026-01-02T06:10:05Z"
                            })
                            .to_string()
                        });
                    let response = format!(
                        "HTTP/1.1 201 Created\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        payload.len(),
                        payload
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        (format!("http://{}", addr), handle)
    }
}
