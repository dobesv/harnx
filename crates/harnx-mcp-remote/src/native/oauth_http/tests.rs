use super::*;
use serde_json::Value;

#[test]
fn test_url_allowed_rules() {
    let loopback_http = Url::parse("http://127.0.0.1:8080/mcp").unwrap();
    let allowed = policy::determine_allowed_classes(&loopback_http);
    let oauth_http = OAuthHttp {
        client: reqwest::Client::new(),
        upstream_scheme: "http".into(),
        upstream_host: "127.0.0.1".into(),
        upstream_port: Some(8080),
        upstream_is_loopback: true,
        allowed_classes: allowed,
        dcr_registration: Arc::new(Mutex::new(None)),
    };

    assert!(oauth_http.is_url_allowed(&Url::parse("http://127.0.0.1:8080/token").unwrap()));
    assert!(!oauth_http.is_url_allowed(&Url::parse("http://127.0.0.1:9090/token").unwrap()));
    assert!(!oauth_http.is_url_allowed(&Url::parse("http://10.0.0.1:8080/token").unwrap()));
    assert!(
        !oauth_http.is_url_allowed(&Url::parse("http://user:pass@127.0.0.1:8080/token").unwrap())
    );
    assert!(!oauth_http.is_url_allowed(&Url::parse("http://127.0.0.1:8080/token#frag").unwrap()));
    assert!(oauth_http.is_url_allowed(&Url::parse("https://auth.example.com/oauth").unwrap()));

    let public_https = Url::parse("https://api.example.com/mcp").unwrap();
    let public_allowed = policy::determine_allowed_classes(&public_https);
    let oauth_public = OAuthHttp {
        upstream_scheme: "https".into(),
        upstream_host: "api.example.com".into(),
        upstream_port: Some(443),
        upstream_is_loopback: false,
        allowed_classes: public_allowed,
        ..oauth_http
    };

    assert!(!oauth_public.is_url_allowed(&Url::parse("http://127.0.0.1:8080/token").unwrap()));
    assert!(!oauth_public.is_url_allowed(&Url::parse("https://10.0.0.1/token").unwrap()));
    assert!(!oauth_public.is_url_allowed(&Url::parse("https://169.254.169.254/token").unwrap()));
    assert!(!oauth_public
        .is_url_allowed(&Url::parse("https://metadata.google.internal/token").unwrap()));
}

#[test]
fn test_sanitize_oauth_error_body() {
    let raw = br#"{"error": "invalid_grant", "error_description": "secret https://corp/123", "secret": "abc"}"#;
    let sanitized = sanitize_oauth_error_body(raw);
    let val: Value = serde_json::from_slice(&sanitized).unwrap();
    assert_eq!(val["error"], "invalid_grant");
    assert_eq!(val["error_description"], "request failed");
    assert!(val.get("secret").is_none());

    let nonstandard = br#"{"error": "custom_internal_code"}"#;
    let sanitized = sanitize_oauth_error_body(nonstandard);
    let val: Value = serde_json::from_slice(&sanitized).unwrap();
    assert_eq!(val["error"], "server_error");

    let html = b"<html>502 Bad Gateway</html>";
    let sanitized = sanitize_oauth_error_body(html);
    let val: Value = serde_json::from_slice(&sanitized).unwrap();
    assert_eq!(val["error"], "server_error");
}

#[tokio::test]
async fn test_account_label_fetch() {
    use axum::{routing::get, Json, Router};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let app = Router::new().route(
        "/userinfo",
        get(|| async {
            Json(serde_json::json!({
                "email": "user@example.test",
                "name": "Test User"
            }))
        }),
    );

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let base_url = Url::parse(&format!("http://127.0.0.1:{}/", addr.port())).unwrap();
    let allowed = policy::determine_allowed_classes(&base_url);
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    let oauth_http = OAuthHttp {
        client,
        upstream_scheme: "http".into(),
        upstream_host: "127.0.0.1".into(),
        upstream_port: Some(addr.port()),
        upstream_is_loopback: true,
        allowed_classes: allowed,
        dcr_registration: Arc::new(Mutex::new(None)),
    };

    let metadata: AuthorizationMetadata = serde_json::from_value(serde_json::json!({
        "authorization_endpoint": "http://127.0.0.1/auth",
        "token_endpoint": "http://127.0.0.1/token",
        "userinfo_endpoint": format!("http://127.0.0.1:{}/userinfo", addr.port())
    }))
    .unwrap();

    let label = oauth_http.account_label(&metadata, "test-token").await;
    assert_eq!(label.as_deref(), Some("user@example.test"));
}
