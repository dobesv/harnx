use super::*;

fn loopback_http(base: &Url) -> Arc<OAuthHttp> {
    Arc::new(OAuthHttp {
        client: reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
        upstream_scheme: "http".into(),
        upstream_host: "127.0.0.1".into(),
        upstream_port: base.port(),
        upstream_is_loopback: true,
        allowed_classes: policy::determine_allowed_classes(base),
        dcr_registration: Arc::new(Mutex::new(None)),
    })
}

#[tokio::test]
async fn discovery_and_token_redirects_never_reach_the_target() {
    use axum::{
        routing::{get, post},
        Router,
    };
    use rmcp::transport::auth::{AuthorizationManager, OAuthClientConfig};
    use std::sync::atomic::{AtomicUsize, Ordering};

    // rmcp follows same-origin discovery redirects itself. Disabling redirects
    // only in reqwest doesn't enforce our policy.
    for location in [
        "/redirect-target",
        "http://example.test/unsafe",
        "https://10.0.0.1/token",
        "http://169.254.169.254/latest/meta-data",
    ] {
        let hits = Arc::new(AtomicUsize::new(0));
        let target_hits = hits.clone();
        let redirect = move || async move {
            (
                axum::http::StatusCode::FOUND,
                [(axum::http::header::LOCATION, location)],
            )
        };
        let app = Router::new()
            .route("/token", post(redirect))
            .route(
                "/redirect-target",
                get(move || {
                    let target_hits = target_hits.clone();
                    async move {
                        target_hits.fetch_add(1, Ordering::SeqCst);
                        "redirect target must not be reached"
                    }
                }),
            )
            .fallback(get(redirect));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let http = loopback_http(&base);
        let manager = AuthorizationManager::new_with_oauth_http_client(base.clone(), http.clone())
            .await
            .unwrap();
        assert!(manager.resolve_metadata().await.is_err());

        // Exercise oauth2's code-exchange path through AuthorizationManager,
        // not a direct reqwest request that bypasses OAuthHttp's checks.
        let mut manager = AuthorizationManager::new_with_oauth_http_client(base.clone(), http)
            .await
            .unwrap();
        manager.set_metadata(
            serde_json::from_value(serde_json::json!({
                "authorization_endpoint": base.join("authorize").unwrap(),
                "token_endpoint": base.join("token").unwrap(),
                "code_challenge_methods_supported": ["S256"]
            }))
            .unwrap(),
        );
        manager
            .configure_client(OAuthClientConfig::new(
                "test-client",
                base.join("callback").unwrap().as_str(),
            ))
            .unwrap();
        let authorization = manager.get_authorization_url(&[]).await.unwrap();
        let state = Url::parse(&authorization)
            .unwrap()
            .query_pairs()
            .find(|(key, _)| key == "state")
            .unwrap()
            .1
            .into_owned();
        assert!(manager
            .exchange_code_for_token("test-code", &state)
            .await
            .is_err());
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        server.abort();
    }
}

#[tokio::test]
async fn blocked_literal_token_endpoint_never_opens_a_socket() {
    use rmcp::transport::auth::{AuthorizationManager, OAuthClientConfig};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = Url::parse(&format!("https://{}/token", listener.local_addr().unwrap())).unwrap();
    let upstream = Url::parse("https://upstream.example/mcp").unwrap();
    let http = Arc::new(OAuthHttp {
        client: reqwest::Client::builder().no_proxy().build().unwrap(),
        upstream_scheme: "https".into(),
        upstream_host: "upstream.example".into(),
        upstream_port: Some(443),
        upstream_is_loopback: false,
        allowed_classes: policy::determine_allowed_classes(&upstream),
        dcr_registration: Arc::new(Mutex::new(None)),
    });
    let mut manager = AuthorizationManager::new_with_oauth_http_client(upstream, http)
        .await
        .unwrap();
    manager.set_metadata(
        serde_json::from_value(serde_json::json!({
            "authorization_endpoint": "https://upstream.example/authorize",
            "token_endpoint": target,
            "code_challenge_methods_supported": ["S256"]
        }))
        .unwrap(),
    );
    manager
        .configure_client(OAuthClientConfig::new(
            "test-client",
            "https://harnx.example/callback",
        ))
        .unwrap();
    let authorization = manager.get_authorization_url(&[]).await.unwrap();
    let state = Url::parse(&authorization)
        .unwrap()
        .query_pairs()
        .find(|(key, _)| key == "state")
        .unwrap()
        .1
        .into_owned();
    assert!(manager
        .exchange_code_for_token("test-code", &state)
        .await
        .is_err());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err()
    );
}
