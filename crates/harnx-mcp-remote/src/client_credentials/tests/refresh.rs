use super::*;
use rmcp::transport::streamable_http_client::StreamableHttpClient;

struct McpServer {
    authorizations: Mutex<Vec<Option<HeaderValue>>>,
    reject_first: bool,
}

async fn mcp_endpoint(
    State(server): State<Arc<McpServer>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let _ = body;
    let mut authorizations = lock(&server.authorizations);
    authorizations.push(headers.get(AUTHORIZATION).cloned());
    let first = authorizations.len() == 1;
    drop(authorizations);
    if first && server.reject_first {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    StatusCode::ACCEPTED.into_response()
}

/// The shared preamble of the adapter tests: a scripted token endpoint, a
/// mock MCP upstream, and the provider behind the adapter.
struct AdapterFixture {
    tokens: TokenFixture,
    mcp: Arc<McpServer>,
    uri: Url,
    client: crate::authenticated_http::AuthenticatedHttp,
    server: JoinHandle<()>,
}

impl Drop for AdapterFixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn adapter_fixture(
    steps: Vec<Step>,
    reject_first: bool,
) -> (AdapterFixture, mpsc::UnboundedReceiver<()>) {
    let (tokens, arrived) = token_server(steps).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = Url::parse(&format!("http://{}/mcp", listener.local_addr().unwrap())).unwrap();
    let mcp = Arc::new(McpServer {
        authorizations: Mutex::new(Vec::new()),
        reject_first,
    });
    let app = Router::new()
        .route("/mcp", any(mcp_endpoint))
        .with_state(mcp.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = crate::authenticated_http::AuthenticatedHttp {
        http: test_client(),
        auth: make_auth(&tokens.endpoint, &[]),
    };
    (
        AdapterFixture {
            tokens,
            mcp,
            uri,
            client,
            server,
        },
        arrived,
    )
}

async fn post_ping(
    client: &crate::authenticated_http::AuthenticatedHttp,
    uri: &Url,
) -> Result<rmcp::transport::streamable_http_client::StreamableHttpPostResponse, String> {
    let ping: rmcp::model::ClientJsonRpcMessage =
        serde_json::from_value(serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}))
            .unwrap();
    client
        .post_message(
            uri.as_str().to_owned().into(),
            ping,
            Some("session-a".into()),
            None,
            std::collections::HashMap::new(),
        )
        .await
        .map_err(|error| error.to_string())
}

#[tokio::test]
async fn adapter_replays_once_with_provider_replacement_after_401() {
    let (adapter, _arrived) = adapter_fixture(
        vec![
            Step::Token {
                token: "t0".into(),
                expires_in: Some(3600),
            },
            Step::Token {
                token: "t1".into(),
                expires_in: Some(30),
            },
        ],
        true,
    )
    .await;

    let response = post_ping(&adapter.client, &adapter.uri)
        .await
        .expect("replay succeeds");
    assert!(matches!(
        response,
        rmcp::transport::streamable_http_client::StreamableHttpPostResponse::Accepted
    ));
    assert_eq!(lock(&adapter.tokens.state.requests).len(), 2);
    let attempts = lock(&adapter.mcp.authorizations);
    assert_eq!(
        attempts.as_slice(),
        &[Some(bearer("t0")), Some(bearer("t1"))],
        "replay must send the installed replacement"
    );
}

#[tokio::test(start_paused = true)]
async fn delayed_401_reuses_replacement_only_until_actual_expiry() {
    let (fixture, _arrived) = token_server(vec![
        Step::Token {
            token: "original".into(),
            expires_in: None,
        },
        Step::Token {
            token: "replacement".into(),
            expires_in: Some(30),
        },
    ])
    .await;
    let auth = make_auth(&fixture.endpoint, &[]);
    let original = ensure_ok(&auth).await;
    auth.refresh_after_rejection(Some(&original)).await.unwrap();

    // Both late 401s name the original. One arrives with a second left
    // on the replacement; the other arrives exactly at its expiry.
    for (advance, expected, grants) in [(29, "replacement", 2), (1, "token-2", 3)] {
        tokio::time::advance(Duration::from_secs(advance)).await;
        auth.refresh_after_rejection(Some(&original)).await.unwrap();
        let mut replay = StreamableHttpClientTransportConfig::with_uri("");
        auth.apply_for_request(&mut replay).unwrap();
        assert_eq!(replay.custom_headers[&AUTHORIZATION], bearer(expected));
        assert_eq!(lock(&fixture.state.requests).len(), grants);
    }
}

#[tokio::test]
async fn initial_stamp_recovers_from_one_retirement_but_not_two() {
    let (adapter, _arrived) = adapter_fixture(Vec::new(), false).await;
    let auth = &adapter.client.auth;
    let first = post_ping(&adapter.client, &adapter.uri);
    let second = post_ping(&adapter.client, &adapter.uri);
    tokio::pin!(first, second);
    assert!(futures::poll!(&mut first).is_pending());
    assert!(futures::poll!(&mut second).is_pending());
    let original = ensure_ok(auth).await;

    // Both POSTs have a successful acquisition pending pickup. Retire that
    // generation before letting either POST stamp it.
    let mut rotate = auth.refresh_after_rejection(Some(&original));
    assert!(futures::poll!(&mut rotate).is_pending());
    assert!(futures::poll!(&mut first).is_pending());
    assert!(futures::poll!(&mut second).is_pending());
    rotate.await.unwrap();
    first.await.expect("send with the shared replacement");

    // The second POST hasn't stamped its replacement yet. A second
    // retirement must fail closed rather than keep reacquiring.
    let replacement = applied_header(auth.as_ref());
    assert_eq!(replacement, bearer("token-1"));
    let mut rotate_again = auth.refresh_after_rejection(Some(&replacement));
    assert!(futures::poll!(&mut rotate_again).is_pending());
    let error = second.await.expect_err("initial stamp retry exhausted");
    assert_eq!(
        error,
        "Client error: client-credentials token is unavailable; retry the call"
    );
    rotate_again.await.unwrap();
    assert_eq!(
        lock(&adapter.mcp.authorizations).as_slice(),
        &[Some(replacement)]
    );
    assert_eq!(lock(&adapter.tokens.state.requests).len(), 3);
}

#[tokio::test]
async fn mandatory_refresh_failure_never_sends_stale_credentials() {
    // The error body echoes the client secret; the failure the adapter
    // surfaces must carry only the status and the allowlisted code.
    let body = format!(r#"{{"error":"invalid_client","error_description":"{SECRET}"}}"#);
    let (adapter, _arrived) = adapter_fixture(
        vec![
            Step::Token {
                token: "stale".into(),
                expires_in: Some(60),
            },
            Step::Raw { status: 500, body },
        ],
        false,
    )
    .await;
    post_ping(&adapter.client, &adapter.uri).await.unwrap();

    let error = post_ping(&adapter.client, &adapter.uri)
        .await
        .expect_err("pre-expiry refresh fails");
    assert!(error.contains("HTTP 500"), "{error}");
    assert!(!error.contains(SECRET), "{error}");
    assert_eq!(
        lock(&adapter.mcp.authorizations).as_slice(),
        &[Some(bearer("stale"))],
        "cache seeding sends once; the failed refresh must not send again"
    );
    assert_eq!(lock(&adapter.tokens.state.requests).len(), 2);
}

#[tokio::test]
async fn concurrent_same_header_401_failure_is_shared_and_retry_recovers() {
    let release = Arc::new(Barrier::new(2));
    let body =
        format!(r#"{{"error":"invalid_client","error_description":"{SECRET} rejected-token"}}"#,);
    let (adapter, mut arrived) = adapter_fixture(
        vec![
            Step::Token {
                token: "rejected-token".into(),
                expires_in: Some(3600),
            },
            Step::Held {
                release: release.clone(),
                then: Box::new(Step::Raw { status: 500, body }),
            },
            Step::Token {
                token: "recovered".into(),
                expires_in: Some(3600),
            },
        ],
        true,
    )
    .await;
    let auth = &adapter.client.auth;
    // Leave this POST waiting to pick up its successful initial grant until
    // the 401 has retired that grant. Its stamp retry must share the failure.
    let before = post_ping(&adapter.client, &adapter.uri);
    tokio::pin!(before);
    assert!(futures::poll!(&mut before).is_pending());
    let rejected = ensure_ok(auth).await;
    arrived.recv().await.unwrap();

    let lead = post_ping(&adapter.client, &adapter.uri);
    tokio::pin!(lead);
    tokio::select! {
        arrival = arrived.recv() => arrival.expect("401 refresh reached token endpoint"),
        _ = &mut lead => panic!("401 request finished before token exchange"),
    }

    // Poll the same-header rejection futures while the endpoint is held.
    // A decision lock held across the exchange would queue them instead.
    let mut first = auth.refresh_after_rejection(Some(&rejected));
    let mut second = auth.refresh_after_rejection(Some(&rejected));
    assert!(futures::poll!(&mut first).is_pending());
    assert!(futures::poll!(&mut second).is_pending());
    let during = post_ping(&adapter.client, &adapter.uri);
    tokio::pin!(during);
    assert!(futures::poll!(&mut before).is_pending());
    assert!(futures::poll!(&mut during).is_pending());

    release.wait().await;
    let error = lead.await.expect_err("failed refresh suppresses replay");
    assert!(error.contains("HTTP 500") && error.contains("invalid_client"));
    assert!(!error.contains(SECRET) && !error.contains("rejected-token"));
    let joined_error = first.await.expect_err("same-header waiter shares failure");
    assert!(error.contains(&joined_error.to_string()));
    assert_eq!(
        second.await.unwrap_err().to_string(),
        joined_error.to_string()
    );
    assert_eq!(before.await.unwrap_err(), error);
    assert_eq!(during.await.unwrap_err(), error);
    assert_eq!(lock(&adapter.tokens.state.requests).len(), 2);
    assert_eq!(
        lock(&adapter.mcp.authorizations).as_slice(),
        &[Some(rejected.clone())],
        "only the request that got 401 reached MCP"
    );

    // Even a request admitted just before the 401 can't stamp the rejected
    // header if it reaches the stamp boundary before recovery.
    let mut config = StreamableHttpClientTransportConfig::with_uri("");
    config
        .custom_headers
        .insert(AUTHORIZATION, rejected.clone());
    assert!(auth.apply_for_request(&mut config).is_err());
    assert!(!config.custom_headers.contains_key(&AUTHORIZATION));

    post_ping(&adapter.client, &adapter.uri).await.unwrap();
    assert_eq!(lock(&adapter.tokens.state.requests).len(), 3);
    assert_eq!(
        lock(&adapter.mcp.authorizations).as_slice(),
        &[Some(rejected), Some(bearer("recovered"))]
    );
}

#[tokio::test]
async fn proactive_cache_hit_does_not_publish_an_attempt_for_mandatory_rejection() {
    let release = Arc::new(Barrier::new(2));
    let (fixture, mut arrived) = token_server(vec![
        Step::Token {
            token: "expired".into(),
            expires_in: Some(0),
        },
        Step::Token {
            token: "fresh-but-rejected".into(),
            expires_in: Some(3600),
        },
        Step::Held {
            release: release.clone(),
            then: Box::new(Step::Token {
                token: "replacement".into(),
                expires_in: Some(30),
            }),
        },
    ])
    .await;
    let provider = make_provider(&fixture.endpoint, &[]);
    provider.ensure_fresh().await.unwrap();
    arrived.recv().await.unwrap();
    provider.ensure_fresh().await.unwrap();
    arrived.recv().await.unwrap();
    provider.ensure_fresh().await.unwrap();

    let rejected = applied_header(&provider);
    assert_eq!(rejected, bearer("fresh-but-rejected"));
    let mut mandatory = provider.refresh_after_rejection(Some(&rejected));
    assert!(futures::poll!(&mut mandatory).is_pending());
    tokio::select! {
        arrival = arrived.recv() => arrival.expect("mandatory refresh reached token endpoint"),
        _ = &mut mandatory => panic!("mandatory refresh finished without an exchange"),
    }
    assert!(futures::poll!(&mut mandatory).is_pending());
    release.wait().await;
    mandatory.await.unwrap();
    assert_eq!(applied_header(&provider), bearer("replacement"));
    assert_eq!(lock(&fixture.state.requests).len(), 3);
}

#[tokio::test]
async fn rejection_joins_real_proactive_exchange_and_invalidates_cache_on_failure() {
    let release = Arc::new(Barrier::new(2));
    let (fixture, mut arrived) = token_server(vec![
        Step::Token {
            token: "expiring".into(),
            expires_in: Some(0),
        },
        Step::Held {
            release: release.clone(),
            then: Box::new(Step::Raw {
                status: 500,
                body: String::new(),
            }),
        },
    ])
    .await;
    let auth = make_auth(&fixture.endpoint, &[]);
    let rejected = ensure_ok(&auth).await;
    arrived.recv().await.unwrap();
    let mut proactive = auth.ensure_fresh();
    tokio::select! {
        arrival = arrived.recv() => arrival.expect("proactive refresh reached token endpoint"),
        _ = &mut proactive => panic!("proactive refresh finished before token exchange"),
    }
    let mut mandatory = auth.refresh_after_rejection(Some(&rejected));
    assert!(futures::poll!(&mut mandatory).is_pending());
    release.wait().await;
    let error = proactive.await.unwrap_err().to_string();
    assert!(error.contains("HTTP 500"));
    assert_eq!(mandatory.await.unwrap_err().to_string(), error);
    assert_eq!(lock(&fixture.state.requests).len(), 2);
    let mut config = StreamableHttpClientTransportConfig::with_uri("");
    assert!(auth.apply_for_request(&mut config).is_err());
    assert_eq!(ensure_ok(&auth).await, bearer("token-2"));
    assert_eq!(lock(&fixture.state.requests).len(), 3);
}
