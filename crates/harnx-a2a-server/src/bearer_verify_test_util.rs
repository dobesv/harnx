use super::*;

use std::sync::atomic::{AtomicU64, Ordering};

use axum::{routing::get, Router};
use jsonwebtoken::{encode, EncodingKey, Header};
use serde_json::json;

pub(crate) const ISSUER: &str = "forge/invocation-token";
pub(crate) const AUDIENCE: &str = "ari:cloud:ecosystem::app/test-app";
pub(crate) const PRIMARY_PEM: &str = include_str!("../tests/fixtures/bearer/primary.pem");
pub(crate) const SECONDARY_PEM: &str = include_str!("../tests/fixtures/bearer/secondary.pem");
pub(crate) const JWKS_PRIMARY: &str = include_str!("../tests/fixtures/bearer/jwks-primary.json");
pub(crate) const JWKS_SECONDARY: &str =
    include_str!("../tests/fixtures/bearer/jwks-secondary.json");

pub(crate) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Forge-style claims: principal, context.cloudId, iss/aud/iat/exp. Audience
/// defaults to AUDIENCE; tests that need a different aud override the claim.
pub(crate) fn forge_claims(principal: &str) -> Value {
    json!({
        "iss": ISSUER,
        "aud": AUDIENCE,
        "iat": now(),
        "exp": now() + 3600,
        "principal": principal,
        "context": {"cloudId": "d0d52620-3203-4cfa-8db5-f2587155f0dd"},
    })
}

/// Signing key bundled as one typed value: CodeScene flags modules where too
/// many function arguments are strings.
pub(crate) struct TestKey {
    pub(crate) kid: &'static str,
    pub(crate) pem: &'static str,
}

pub(crate) const PRIMARY_KEY: TestKey = TestKey {
    kid: "primary-2026-01",
    pem: PRIMARY_PEM,
};
pub(crate) const SECONDARY_KEY: TestKey = TestKey {
    kid: "secondary-2026-01",
    pem: SECONDARY_PEM,
};

pub(crate) fn sign(claims: &Value, key: &TestKey) -> String {
    let encoding = EncodingKey::from_rsa_pem(key.pem.as_bytes()).unwrap();
    encode(
        &Header {
            alg: Algorithm::RS256,
            kid: Some(key.kid.into()),
            ..Default::default()
        },
        claims,
        &encoding,
    )
    .unwrap()
}

/// JWKS endpoint URL of a test server; newtyped so the verifier helpers take
/// a typed value instead of a raw string.
pub(crate) struct JwksUrl(pub(crate) String);

/// Bind first so connections queue in the backlog before the server runs.
pub(crate) async fn serve(app: Router) -> JwksUrl {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/.well-known/jwks.json",
        listener.local_addr().unwrap()
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    JwksUrl(url)
}

/// Local JWKS server that counts fetches and can swap its key set.
pub(crate) struct JwksServer {
    pub(crate) url: JwksUrl,
    body: std::sync::Arc<std::sync::Mutex<&'static str>>,
    fetches: std::sync::Arc<AtomicU64>,
}

impl JwksServer {
    pub(crate) async fn start(body: &'static str) -> Self {
        let fetches = Arc::new(AtomicU64::new(0));
        let body = Arc::new(std::sync::Mutex::new(body));
        let counter = fetches.clone();
        let current = body.clone();
        let app = Router::new().route(
            "/.well-known/jwks.json",
            get(move || {
                counter.fetch_add(1, Ordering::SeqCst);
                let body = *current.lock().unwrap();
                async move { axum::Json(serde_json::from_str::<Value>(body).unwrap()) }
            }),
        );
        Self {
            url: serve(app).await,
            body,
            fetches,
        }
    }

    pub(crate) fn swap(&self, body: &'static str) {
        *self.body.lock().unwrap() = body;
    }

    pub(crate) fn fetches(&self) -> u64 {
        self.fetches.load(Ordering::SeqCst)
    }
}

pub(crate) fn verifier(url: &JwksUrl) -> BearerVerifier {
    BearerVerifier::new(BearerVerificationConfig {
        jwks_url: url.0.clone(),
        issuer: ISSUER.to_owned(),
        audience: AUDIENCE.to_owned(),
    })
    .unwrap()
}

/// Replace the internal clock so the 30 s refetch throttle can be tested
/// without sleeping; returns the advance-by-milliseconds control.
pub(crate) fn verifier_with_fake_clock(url: &JwksUrl) -> (BearerVerifier, impl Fn(u64)) {
    let millis = Arc::new(AtomicU64::new(0));
    let mut verifier = verifier(url);
    verifier.clock = {
        let millis = millis.clone();
        Arc::new(move || millis.load(Ordering::SeqCst))
    };
    let advance = move |by: u64| {
        millis.fetch_add(by, Ordering::SeqCst);
    };
    (verifier, advance)
}

#[cfg(test)]
mod tests {
    use super::test_util::{
        forge_claims, now, serve, sign, verifier, verifier_with_fake_clock, JwksServer, AUDIENCE,
        JWKS_PRIMARY, JWKS_SECONDARY, PRIMARY_PEM, SECONDARY_PEM,
    };
    use super::*;

    use axum::{routing::get, Router};
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use serde_json::json;

    fn kidless_header(token: &str) -> String {
        // Rebuild the token with a well-formed JOSE header that has no kid.
        let mut segments = token.split('.');
        let (_header, payload, signature) = (
            segments.next().unwrap(),
            segments.next().unwrap(),
            segments.next().unwrap(),
        );
        format!(
            "{}.{}.{}",
            URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#),
            payload,
            signature
        )
    }

    #[tokio::test]
    async fn verify_valid_token_and_cache_key_set() {
        let server = JwksServer::start(JWKS_PRIMARY).await;
        let verifier = verifier(&server.url);
        let token = sign(&forge_claims("557058:account-id"), &PRIMARY_KEY);
        let verified = verifier.verify(&token).await.unwrap();
        assert_eq!(verified["principal"], "557058:account-id");
        // The key set is cached: a second verification doesn't refetch.
        verifier.verify(&token).await.unwrap();
        assert_eq!(server.fetches(), 1);
    }

    #[tokio::test]
    async fn verify_bad_signature_and_wrong_audience_fail_closed() {
        let server = JwksServer::start(JWKS_PRIMARY).await;
        let verifier = verifier(&server.url);
        let forged = sign(
            &forge_claims("account-id"),
            &TestKey {
                kid: "primary-2026-01",
                pem: SECONDARY_PEM,
            },
        );
        assert_eq!(
            verifier.verify(&forged).await.unwrap_err(),
            VerifyFailure::BadSignature
        );
        let mut other_aud = forge_claims("account-id");
        other_aud["aud"] = json!("ari:cloud:ecosystem::app/other");
        let wrong_audience = sign(&other_aud, &PRIMARY_KEY);
        assert_eq!(
            verifier.verify(&wrong_audience).await.unwrap_err(),
            VerifyFailure::BadAudience
        );
    }

    #[tokio::test]
    async fn verify_missing_kid_rejects_without_fetch() {
        let server = JwksServer::start(JWKS_PRIMARY).await;
        let verifier = verifier(&server.url);
        let token = sign(&forge_claims("account-id"), &PRIMARY_KEY);
        assert_eq!(
            verifier.verify(&kidless_header(&token)).await.unwrap_err(),
            VerifyFailure::MissingKid
        );
        assert_eq!(server.fetches(), 0);
    }

    #[tokio::test]
    async fn verify_future_issued_at_rejected() {
        let server = JwksServer::start(JWKS_PRIMARY).await;
        let verifier = verifier(&server.url);
        let mut future = forge_claims("account-id");
        future["iat"] = json!(now() + 3600);
        future["exp"] = json!(now() + 7200);
        let token = sign(&future, &PRIMARY_KEY);
        assert_eq!(
            verifier.verify(&token).await.unwrap_err(),
            VerifyFailure::FutureIssuedAt
        );
    }

    #[tokio::test]
    async fn verify_fractional_and_wrong_typed_iat_fail_closed() {
        let server = JwksServer::start(JWKS_PRIMARY).await;
        let verifier = verifier(&server.url);
        // A fractional NumericDate in the future beyond leeway is rejected.
        let mut future = forge_claims("account-id");
        future["iat"] = json!(now() as f64 + 3600.5);
        future["exp"] = json!(now() + 7200);
        let token = sign(&future, &PRIMARY_KEY);
        assert_eq!(
            verifier.verify(&token).await.unwrap_err(),
            VerifyFailure::FutureIssuedAt
        );

        // A current fractional NumericDate is a valid iat.
        let mut fractional = forge_claims("account-id");
        fractional["iat"] = json!(now() as f64 + 0.25);
        let token = sign(&fractional, &PRIMARY_KEY);
        verifier.verify(&token).await.unwrap();

        // A present iat that isn't a JSON number fails closed, not skipped.
        for iat in [
            json!("1700000000"),
            json!(true),
            json!(null),
            json!([1700000000]),
            json!({"seconds": 1700000000}),
        ] {
            let mut wrong = forge_claims("account-id");
            wrong["iat"] = iat.clone();
            let token = sign(&wrong, &PRIMARY_KEY);
            assert_eq!(
                verifier.verify(&token).await.unwrap_err(),
                VerifyFailure::Malformed,
                "iat {iat}"
            );
        }
    }

    #[test]
    fn verifier_requires_https_except_loopback() {
        fn config(jwks_url: &str) -> BearerVerificationConfig {
            BearerVerificationConfig {
                jwks_url: jwks_url.to_owned(),
                issuer: ISSUER.to_owned(),
                audience: AUDIENCE.to_owned(),
            }
        }
        for url in [
            "http://example.com/.well-known/jwks.json",
            "http://192.168.1.10/.well-known/jwks.json",
            "ftp://example.com/.well-known/jwks.json",
        ] {
            let error = BearerVerifier::new(config(url)).unwrap_err();
            let message = format!("{error:#}");
            assert!(message.contains("https"), "{url}: {message}");
        }
        for url in [
            "https://example.com/.well-known/jwks.json",
            // Loopback http is allowed so tests can serve a local JWKS.
            "http://127.0.0.1:39001/.well-known/jwks.json",
            "http://localhost/.well-known/jwks.json",
        ] {
            BearerVerifier::new(config(url)).unwrap();
        }
    }

    #[tokio::test]
    async fn verify_unknown_kid_refetches_and_rotation_replaces_keys() {
        let server = JwksServer::start(JWKS_PRIMARY).await;
        let (verifier, advance) = verifier_with_fake_clock(&server.url);
        let primary = sign(&forge_claims("account-id"), &PRIMARY_KEY);
        verifier.verify(&primary).await.unwrap();
        assert_eq!(server.fetches(), 1);

        // A new key verifies after one refetch.
        server.swap(JWKS_SECONDARY);
        let rotated = sign(&forge_claims("account-id"), &SECONDARY_KEY);
        assert_eq!(
            verifier.verify(&rotated).await.unwrap_err(),
            VerifyFailure::UnknownKid
        );
        advance(REFETCH_THROTTLE.as_millis() as u64 + 1);
        verifier.verify(&rotated).await.unwrap();
        assert_eq!(server.fetches(), 2);

        // The refresh replaced the whole set, so the removed key no longer verifies.
        assert_eq!(
            verifier.verify(&primary).await.unwrap_err(),
            VerifyFailure::UnknownKid
        );
    }

    #[tokio::test]
    async fn verify_refetch_is_single_flight_and_throttled() {
        let server = JwksServer::start(JWKS_PRIMARY).await;
        let (verifier, advance) = verifier_with_fake_clock(&server.url);
        let verifier = Arc::new(verifier);
        let garbage = sign(
            &forge_claims("account-id"),
            &TestKey {
                kid: "garbage-kid",
                pem: PRIMARY_PEM,
            },
        );

        // Concurrent unknown-kid misses share one fetch.
        let mut concurrent = Vec::new();
        for _ in 0..8 {
            let verifier = verifier.clone();
            let token = garbage.clone();
            concurrent.push(tokio::spawn(async move { verifier.verify(&token).await }));
        }
        for result in concurrent {
            assert_eq!(
                result.await.unwrap().unwrap_err(),
                VerifyFailure::UnknownKid
            );
        }
        assert_eq!(server.fetches(), 1);

        // Within the throttle window, further unknown kids fail closed without fetching.
        assert_eq!(
            verifier.verify(&garbage).await.unwrap_err(),
            VerifyFailure::UnknownKid
        );
        assert_eq!(server.fetches(), 1);

        advance(REFETCH_THROTTLE.as_millis() as u64 + 1);
        assert_eq!(
            verifier.verify(&garbage).await.unwrap_err(),
            VerifyFailure::UnknownKid
        );
        assert_eq!(server.fetches(), 2);
    }

    #[tokio::test]
    async fn verify_jwks_failures_fail_closed() {
        // Unreachable endpoint: fetch error.
        let unreachable = verifier(&JwksUrl("http://127.0.0.1:1/.well-known/jwks.json".into()));
        let token = sign(&forge_claims("account-id"), &PRIMARY_KEY);
        assert_eq!(
            unreachable.verify(&token).await.unwrap_err(),
            VerifyFailure::JwksFetch
        );

        // Non-200 response.
        let app = Router::new().route(
            "/.well-known/jwks.json",
            get(|| async { axum::http::StatusCode::INTERNAL_SERVER_ERROR }),
        );
        let failing = verifier(&serve(app).await);
        assert_eq!(
            failing.verify(&token).await.unwrap_err(),
            VerifyFailure::JwksFetch
        );

        // Unparseable body.
        let app = Router::new().route("/.well-known/jwks.json", get(|| async { "not json" }));
        let unparseable = verifier(&serve(app).await);
        assert_eq!(
            unparseable.verify(&token).await.unwrap_err(),
            VerifyFailure::JwksBody
        );

        // Response over the size cap.
        let app = Router::new().route(
            "/.well-known/jwks.json",
            get(|| async { "x".repeat(MAX_JWKS_BYTES + 1) }),
        );
        let oversized = verifier(&serve(app).await);
        assert_eq!(
            oversized.verify(&token).await.unwrap_err(),
            VerifyFailure::JwksBody
        );

        // Redirects are refused outright; no scheme downgrade or re-pointing.
        let app = Router::new().route(
            "/.well-known/jwks.json",
            get(|| async { axum::response::Redirect::temporary("/moved-jwks") }),
        );
        let redirecting = verifier(&serve(app).await);
        assert_eq!(
            redirecting.verify(&token).await.unwrap_err(),
            VerifyFailure::JwksFetch
        );
    }

    #[test]
    fn verify_failure_categories_never_leak_values() {
        // Categories are static strings; assert the redaction contract holds.
        for failure in [
            VerifyFailure::Malformed,
            VerifyFailure::UnsupportedAlgorithm,
            VerifyFailure::MissingKid,
            VerifyFailure::UnknownKid,
            VerifyFailure::JwksFetch,
            VerifyFailure::JwksBody,
            VerifyFailure::InvalidKey,
            VerifyFailure::BadSignature,
            VerifyFailure::Expired,
            VerifyFailure::NotYetValid,
            VerifyFailure::BadIssuer,
            VerifyFailure::BadAudience,
            VerifyFailure::MissingClaim,
            VerifyFailure::FutureIssuedAt,
        ] {
            let category = failure.category();
            assert!(!category.contains("primary-2026-01"), "{category}");
            assert!(!category.contains("account-id"), "{category}");
            assert!(!category.contains("Bearer "), "{category}");
        }
        assert_eq!(
            VerifyFailure::BadSignature.category(),
            "bad bearer token signature"
        );
        assert!(VerifyFailure::JwksFetch.is_jwks_fault());
        assert!(VerifyFailure::JwksBody.is_jwks_fault());
        assert!(!VerifyFailure::InvalidKey.is_jwks_fault());
    }
}
