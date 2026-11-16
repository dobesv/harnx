//! Optional bearer token verification for bearer identity sources.
//!
//! Off unless `--bearer-jwks-url` is configured. When enabled, bearer identity
//! sources (`forge-fit`, `bearer-claim:CLAIM`) resolve only from tokens that
//! verify: RS256 signature against a JWKS, plus `iss`, `aud`, `exp`, `nbf` and
//! `iat`. Failures close the request (401 / JSON-RPC -32000) with a reason
//! category; tokens, claims and JWKS bodies are never logged or surfaced.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use jsonwebtoken::{
    jwk::{AlgorithmParameters, Jwk, JwkSet},
    Algorithm, DecodingKey, Validation,
};
use serde_json::{Map, Value};
use tokio::sync::Mutex;

/// Clock-skew tolerance for time claims, in seconds. FIT lifetimes are ~25 s,
/// so the library default of 60 s would be too loose.
const LEEWAY_SECONDS: u64 = 10;

/// At most one JWKS refetch per throttle window, for unknown `kid`s or expired
/// sets. Both fail closed within the window to prevent fetch storms.
const REFETCH_THROTTLE: Duration = Duration::from_secs(30);

/// Bound trust in removed keys even when their `kid` keeps matching. Expired
/// sets fail closed if refresh fails: an outage must not extend trust in a
/// potentially compromised key. Single-flight and the attempt throttle still
/// bound refresh traffic during an outage; there is no stale-key grace period.
const JWKS_CACHE_MAX_AGE: Duration = Duration::from_secs(10 * 60);

const JWKS_FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// JWKS response cap; a valid Forge key set is a few KiB.
const MAX_JWKS_BYTES: usize = 1024 * 1024;

/// Milliseconds since verifier construction. Injectable so cache expiry and
/// the 30 s refetch throttle can be tested without sleeping.
type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

fn monotonic_clock() -> Clock {
    let start = std::time::Instant::now();
    Arc::new(move || start.elapsed().as_millis() as u64)
}

/// Loopback host check for the http-scheme exception in `BearerVerifier::new`.
fn is_loopback(url: &reqwest::Url) -> bool {
    match url.host_str() {
        Some("localhost") | Some("[::1]") | Some("::1") => true,
        Some(host) => host
            .parse::<std::net::Ipv4Addr>()
            .is_ok_and(|ip| ip.is_loopback()),
        None => false,
    }
}

/// Why a bearer token was rejected. Carries reason categories only, never
/// token, claim or JWKS content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifyFailure {
    Malformed,
    UnsupportedAlgorithm,
    MissingKid,
    UnknownKid,
    JwksUnavailable,
    JwksFetch,
    JwksBody,
    InvalidKey,
    BadSignature,
    Expired,
    NotYetValid,
    BadIssuer,
    BadAudience,
    MissingClaim,
    FutureIssuedAt,
}

impl VerifyFailure {
    /// Short static reason category, safe for error messages and logs.
    pub fn category(self) -> &'static str {
        match self {
            Self::Malformed => "malformed bearer token",
            Self::UnsupportedAlgorithm => "unsupported bearer token algorithm",
            Self::MissingKid => "bearer token has no key id",
            Self::UnknownKid => "bearer token key id is unknown",
            Self::JwksUnavailable => "jwks unavailable (expired set, refresh throttled)",
            Self::JwksFetch => "jwks fetch failed",
            Self::JwksBody => "jwks response is invalid",
            Self::InvalidKey => "jwks key is invalid",
            Self::BadSignature => "bad bearer token signature",
            Self::Expired => "bearer token is expired",
            Self::NotYetValid => "bearer token is not yet valid",
            Self::BadIssuer => "bearer token issuer is invalid",
            Self::BadAudience => "bearer token audience is invalid",
            Self::MissingClaim => "bearer token is missing a required claim",
            Self::FutureIssuedAt => "bearer token is issued in the future",
        }
    }

    /// JWKS fetch-path faults, logged at `warn`; these occur during a fetch
    /// and are bounded by the refetch throttle. Per-token rejections stay at
    /// `debug` so forged-token noise doesn't drown the log.
    pub fn is_jwks_fault(self) -> bool {
        matches!(self, Self::JwksFetch | Self::JwksBody)
    }

    fn from_jsonwebtoken(error: jsonwebtoken::errors::Error) -> Self {
        use jsonwebtoken::errors::ErrorKind;
        match error.kind() {
            ErrorKind::InvalidSignature => Self::BadSignature,
            ErrorKind::ExpiredSignature => Self::Expired,
            ErrorKind::ImmatureSignature => Self::NotYetValid,
            ErrorKind::InvalidIssuer => Self::BadIssuer,
            ErrorKind::InvalidAudience => Self::BadAudience,
            ErrorKind::InvalidAlgorithm | ErrorKind::InvalidAlgorithmName => {
                Self::UnsupportedAlgorithm
            }
            ErrorKind::MissingRequiredClaim(_) => Self::MissingClaim,
            _ => Self::Malformed,
        }
    }
}

struct JwksCache {
    /// The whole fetched key set; a refresh replaces it, so rotated-out keys disappear.
    keys: Option<Arc<JwkSet>>,
    /// When the last fetch attempt started, including failed or cancelled fetches.
    last_fetch: Option<u64>,
    /// When the current key set was fetched successfully, for cache expiry.
    fetched_at: Option<u64>,
}

/// Config only, never token or JWKS state.
impl std::fmt::Debug for BearerVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BearerVerifier")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

/// Settings from `--bearer-jwks-url`, `--bearer-issuer` and `--bearer-audience`.
pub struct BearerVerificationConfig {
    pub jwks_url: String,
    pub issuer: String,
    pub audience: String,
}

/// Verifies bearer tokens against a JWKS. Share one instance; the key cache
/// and the single-flight refetch are per instance.
pub struct BearerVerifier {
    url: reqwest::Url,
    validation: Validation,
    client: reqwest::Client,
    state: std::sync::Mutex<JwksCache>,
    /// Held only while a refetch is in flight, so concurrent unknown-`kid`
    /// misses or expired-cache requests share one fetch.
    fetch_lock: Mutex<()>,
    clock: Clock,
}

impl BearerVerifier {
    pub fn new(config: BearerVerificationConfig) -> anyhow::Result<Self> {
        let BearerVerificationConfig {
            jwks_url,
            issuer,
            audience,
        } = config;
        let url: reqwest::Url = jwks_url
            .parse()
            .with_context(|| format!("invalid --bearer-jwks-url '{jwks_url}'"))?;
        // The JWKS is the trust root for bearer identities: a plaintext fetch
        // lets an on-path attacker substitute keys. http is allowed only on
        // loopback hosts so tests can serve a local JWKS.
        anyhow::ensure!(
            url.scheme() == "https" || url.scheme() == "http" && is_loopback(&url),
            "--bearer-jwks-url must use https; http is only allowed on loopback hosts"
        );
        let client = reqwest::Client::builder()
            .timeout(JWKS_FETCH_TIMEOUT)
            // Refuse redirects outright so an https endpoint can't be downgraded
            // to a plaintext hop (or pointed anywhere else).
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("build JWKS HTTP client")?;
        let mut validation = Validation::new(Algorithm::RS256);
        validation.leeway = LEEWAY_SECONDS;
        // jsonwebtoken defaults validate_nbf to false.
        validation.validate_nbf = true;
        validation.set_issuer(&[issuer]);
        validation.set_audience(&[audience]);
        validation.set_required_spec_claims(&["exp", "iss", "aud"]);
        Ok(Self {
            url,
            validation,
            client,
            state: std::sync::Mutex::new(JwksCache {
                keys: None,
                last_fetch: None,
                fetched_at: None,
            }),
            fetch_lock: Mutex::new(()),
            clock: monotonic_clock(),
        })
    }

    /// Verify a bearer token and return its claims. Never persist or log the
    /// token or the claims; persist only the identity derived from them.
    pub async fn verify(&self, token: &str) -> Result<Map<String, Value>, VerifyFailure> {
        let header = jsonwebtoken::decode_header(token).map_err(|_| VerifyFailure::Malformed)?;
        if header.alg != Algorithm::RS256 {
            return Err(VerifyFailure::UnsupportedAlgorithm);
        }
        let Some(kid) = header.kid else {
            return Err(VerifyFailure::MissingKid);
        };
        let keys = self.keys(&kid).await?;
        let Some(jwk) = keys.find(&kid) else {
            return Err(VerifyFailure::UnknownKid);
        };
        let key = decoding_key(jwk)?;
        let data = jsonwebtoken::decode::<Value>(token, &key, &self.validation)
            .map_err(VerifyFailure::from_jsonwebtoken)?;
        // jsonwebtoken doesn't validate iat; reject tokens issued in the
        // future. A present iat must be a numeric NumericDate, compared as
        // f64 so fractional values can't slip past; any other type fails closed.
        match data.claims.get("iat") {
            None => {}
            Some(Value::Number(number)) => {
                let Some(iat) = number.as_f64() else {
                    return Err(VerifyFailure::Malformed);
                };
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_secs_f64())
                    .unwrap_or_default();
                if iat > now + LEEWAY_SECONDS as f64 {
                    return Err(VerifyFailure::FutureIssuedAt);
                }
            }
            Some(_) => return Err(VerifyFailure::Malformed),
        }
        match data.claims {
            Value::Object(claims) => Ok(claims),
            _ => Err(VerifyFailure::Malformed),
        }
    }

    /// A fresh key set containing `kid`, refetching on expiry or an unknown key.
    async fn keys(&self, kid: &str) -> Result<Arc<JwkSet>, VerifyFailure> {
        if let Some(keys) = self.cached(kid) {
            return Ok(keys);
        }
        let _flight = self.fetch_lock.lock().await;
        // Another waiter on the same fetch may have refreshed the set already.
        if let Some(keys) = self.cached(kid) {
            return Ok(keys);
        }
        {
            let mut state = self.state.lock().unwrap();
            let now = (self.clock)();
            if let Some(last) = state.last_fetch {
                if now.saturating_sub(last) < REFETCH_THROTTLE.as_millis() as u64 {
                    let expired = state.fetched_at.is_some_and(|fetched_at| {
                        now.saturating_sub(fetched_at) >= JWKS_CACHE_MAX_AGE.as_millis() as u64
                    });
                    return Err(if expired {
                        VerifyFailure::JwksUnavailable
                    } else {
                        VerifyFailure::UnknownKid
                    });
                }
            }
            // Record before awaiting: cancellation must not bypass the throttle.
            state.last_fetch = Some(now);
        }
        let keys = self.fetch().await?;
        let mut state = self.state.lock().unwrap();
        state.fetched_at = Some((self.clock)());
        state.keys = Some(keys.clone());
        if keys.find(kid).is_some() {
            Ok(keys)
        } else {
            Err(VerifyFailure::UnknownKid)
        }
    }

    fn cached(&self, kid: &str) -> Option<Arc<JwkSet>> {
        let state = self.state.lock().unwrap();
        let age = (self.clock)().saturating_sub(state.fetched_at?);
        if age >= JWKS_CACHE_MAX_AGE.as_millis() as u64 {
            return None;
        }
        state
            .keys
            .as_ref()
            .filter(|keys| keys.find(kid).is_some())
            .cloned()
    }

    async fn fetch(&self) -> Result<Arc<JwkSet>, VerifyFailure> {
        let response = self
            .client
            .get(self.url.clone())
            .send()
            .await
            .map_err(|_| VerifyFailure::JwksFetch)?;
        if !response.status().is_success() {
            return Err(VerifyFailure::JwksFetch);
        }
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        use futures::StreamExt;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| VerifyFailure::JwksFetch)?;
            if body.len() + chunk.len() > MAX_JWKS_BYTES {
                return Err(VerifyFailure::JwksBody);
            }
            body.extend_from_slice(&chunk);
        }
        let keys: JwkSet = serde_json::from_slice(&body).map_err(|_| VerifyFailure::JwksBody)?;
        Ok(Arc::new(keys))
    }
}

/// Build a decoding key, tolerating `=` padding on RSA components. JWK values
/// are unpadded base64url (RFC 7518 §6.3.1.1), but Forge's JWKS pads `n`, and
/// `DecodingKey::from_jwk` rejects padding (#2712).
fn decoding_key(jwk: &Jwk) -> Result<DecodingKey, VerifyFailure> {
    match &jwk.algorithm {
        AlgorithmParameters::RSA(params) => DecodingKey::from_rsa_components(
            params.n.trim_end_matches('='),
            params.e.trim_end_matches('='),
        ),
        _ => DecodingKey::from_jwk(jwk),
    }
    .map_err(|_| VerifyFailure::InvalidKey)
}

#[cfg(test)]
#[path = "bearer_verify_test_util.rs"]
pub(crate) mod test_util;
