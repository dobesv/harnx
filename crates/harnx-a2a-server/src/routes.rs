//! Dispatch percent-decoded export names to routers with export-specific state.

use a2a_server_lf::jsonrpc::jsonrpc_router;
use axum::{
    extract::{Path, Request, State},
    http::{StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::any,
    Router,
};
use std::{collections::HashMap, sync::Arc};
use tower::ServiceExt;

use crate::{agent_card, exports::Export, identity, web_url};

/// Build public discovery and JSON-RPC routes per export, shared by its lookup keys.
pub fn router<H: a2a_server_lf::handler::RequestHandler>(
    exports: &[Export],
    public_base_url: Option<&str>,
    user_id_headers: &[String],
    handler: impl Fn(&Export, identity::Identity) -> Arc<H>,
) -> anyhow::Result<Router> {
    let public_base_url = web_url::normalize_public_base_url(public_base_url)?;
    let identity = identity::Identity::new(user_id_headers)?;
    Ok(router_with(exports, |export| {
        // Layer RPC before merging discovery: cards must remain public.
        jsonrpc_router(handler(export, identity.clone()))
            .route_layer(axum::middleware::from_fn(crate::sse::headers))
            .route_layer(axum::middleware::from_fn(crate::compat::normalize))
            .route_layer(axum::middleware::from_fn_with_state(
                identity.clone(),
                identity::require_identity,
            ))
            .merge(agent_card::router(export, public_base_url.as_deref()))
    }))
}

fn router_with(exports: &[Export], mut export_router: impl FnMut(&Export) -> Router) -> Router {
    let mut routers = HashMap::new();
    for export in exports {
        let router = export_router(export);
        for key in &export.lookup_keys {
            routers.insert(key.clone(), router.clone());
        }
    }
    Router::new()
        .route("/agents/{name}", any(dispatch))
        .route("/agents/{name}/", any(dispatch))
        .route("/agents/{name}/{*path}", any(dispatch))
        .with_state(Arc::new(routers))
}

async fn dispatch(
    State(routers): State<Arc<HashMap<String, Router>>>,
    Path(params): Path<HashMap<String, String>>,
    mut request: Request,
) -> Response {
    let Some(router) = params.get("name").and_then(|name| routers.get(name)) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    // Strip the raw prefix, not the decoded name: %2F must stay within the
    // export segment. Preserve headers, query, and body (including SSE input).
    let remainder = request.uri().path().splitn(4, '/').nth(3).unwrap_or("");
    let mut path_and_query = format!("/{remainder}");
    if let Some(query) = request.uri().query() {
        path_and_query.push('?');
        path_and_query.push_str(query);
    }
    let mut parts = request.uri().clone().into_parts();
    let Ok(path_and_query) = path_and_query.parse() else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    parts.path_and_query = Some(path_and_query);
    let Ok(uri) = Uri::from_parts(parts) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    *request.uri_mut() = uri;
    match router.clone().oneshot(request).await {
        Ok(response) => response,
        Err(error) => match error {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{cli::AgentSpec, exports::resolve_exports, test_support::TestConfigSandbox};
    use axum::{
        body::Body,
        http::Request,
        routing::{get, post},
    };
    use http_body_util::BodyExt;

    fn router(
        exports: &[Export],
        base: Option<&str>,
        headers: &[String],
    ) -> anyhow::Result<Router> {
        super::router(exports, base, headers, |_, _| {
            Arc::new(crate::test_support::RoutingHandler)
        })
    }

    async fn exports(sandbox: &TestConfigSandbox) -> Vec<Export> {
        sandbox.write_agent_with_front_matter(
            "pkg/agent",
            "description: Package agent\nversion: 2.3.4\nconversation_starters:\n  - Review the checkout flow",
            "Prompt",
        );
        sandbox.write_agent_with_front_matter("a__b", "description: ''", "Prompt");
        resolve_exports(
            &[
                AgentSpec {
                    name: "pkg/agent".into(),
                    alias: Some("tools".into()),
                },
                AgentSpec {
                    name: "a__b".into(),
                    alias: None,
                },
            ],
            None,
            Some(sandbox.config_dir()),
        )
        .await
        .unwrap()
    }

    fn request(uri: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .header("a2a-version", "1.0")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","id":1,"method":"GetTask","params":{"id":"task"}}"#,
            ))
            .unwrap()
    }

    #[tokio::test]
    async fn identity_rpc_missing_header_rejected_for_every_method() {
        let sandbox = TestConfigSandbox::new();
        let exports = exports(&sandbox).await;
        let app = router(&exports, None, &["X-User-ID".into()]).unwrap();
        for method in [
            "SendMessage",
            "SendStreamingMessage",
            "GetTask",
            "CancelTask",
            "SubscribeToTask",
            "ListTasks",
            "CreateTaskPushNotificationConfig",
            "GetTaskPushNotificationConfig",
            "ListTaskPushNotificationConfigs",
            "DeleteTaskPushNotificationConfig",
            "GetExtendedAgentCard",
            "message/send",
            "tasks/get",
            "UnknownMethod",
        ] {
            for name in ["tools", "pkg__agent", "pkg%2Fagent", "a__b"] {
                for suffix in ["", "/"] {
                    // No A2A-Version either: identity must precede protocol dispatch.
                    let req = Request::builder()
                        .method("POST")
                        .uri(format!("/agents/{name}{suffix}"))
                        .header("content-type", "application/json")
                        .body(Body::from(
                            serde_json::json!({
                                "jsonrpc": "2.0", "id": "identity-request", "method": method,
                                "params": {},
                            })
                            .to_string(),
                        ))
                        .unwrap();
                    let response = app.clone().oneshot(req).await.unwrap();
                    assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{method}");
                    assert_eq!(response.headers()["content-type"], "application/json");
                    let body = response.into_body().collect().await.unwrap().to_bytes();
                    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    assert_eq!(
                        json,
                        serde_json::json!({
                            "jsonrpc": "2.0", "id": "identity-request",
                            "error": {"code": identity::MISSING_IDENTITY_CODE,
                                "message": "missing or empty user identity header"},
                        })
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn identity_card_routes_stay_public_and_valid_rpc_headers_pass() {
        let sandbox = TestConfigSandbox::new();
        let exports = exports(&sandbox).await;
        let app = router(&exports, None, &["X-User-ID".into()]).unwrap();
        assert_cards(
            app.clone(),
            &exports,
            &[("host", "public.example")],
            "http://public.example",
        )
        .await;
        for value in ["", "  ", " , ignored", " user , ignored"] {
            let mut req = request("/agents/tools");
            req.headers_mut()
                .insert("x-user-id", value.parse().unwrap());
            let response = app.clone().oneshot(req).await.unwrap();
            let expected = if value.starts_with(" user") {
                StatusCode::OK
            } else {
                StatusCode::UNAUTHORIZED
            };
            assert_eq!(response.status(), expected);
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(json["id"], 1);
            if expected == StatusCode::OK {
                assert_eq!(
                    json["error"]["message"],
                    "A2A agent execution is not implemented yet"
                );
            } else {
                assert_eq!(json["error"]["code"], identity::MISSING_IDENTITY_CODE);
            }
        }
        assert_eq!(
            app.oneshot(request("/agents/unknown"))
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn export_routes_forward_all_lookup_keys_to_jsonrpc() {
        let sandbox = TestConfigSandbox::new();
        let exports = exports(&sandbox).await;
        let app = router(&exports, None, &[]).unwrap();
        for name in ["pkg%2Fagent", "pkg__agent", "tools", "a__b"] {
            for suffix in ["", "/"] {
                let response = app
                    .clone()
                    .oneshot(request(&format!("/agents/{name}{suffix}")))
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK, "{name}{suffix}");
                let body = response.into_body().collect().await.unwrap().to_bytes();
                let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(json["id"], 1);
                assert_eq!(
                    json["error"]["message"],
                    "A2A agent execution is not implemented yet"
                );
            }
        }
        for uri in [
            "/agents/unknown",
            "/agents/unknown/",
            "/agents/a%2Fb",
            "/agents/pkg/agent",
            "/not-an-agent",
        ] {
            let response = app.clone().oneshot(request(uri)).await.unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
        }
    }

    #[tokio::test]
    async fn export_routes_share_export_state_and_preserve_subpaths() {
        let sandbox = TestConfigSandbox::new();
        let exports = exports(&sandbox).await;
        let mut built = 0;
        let app = router_with(&exports, |export| {
            built += 1;
            let agent = export.agent.clone();
            let handler = move |req: Request<Body>| {
                let agent = agent.clone();
                async move {
                    assert_eq!(req.headers()["x-test"], "preserved");
                    format!("{agent}:{}", req.uri())
                }
            };
            Router::new()
                .route("/", post(handler.clone()))
                .route("/.well-known/agent-card.json", get(handler.clone()))
                .route("/.well-known/agent.json", get(handler))
        });
        assert_eq!(built, 2);
        for (name, agent) in [
            ("pkg%2Fagent", "pkg/agent"),
            ("pkg__agent", "pkg/agent"),
            ("tools", "pkg/agent"),
            ("a__b", "a__b"),
        ] {
            for path in [
                "",
                "/",
                "/.well-known/agent-card.json",
                "/.well-known/agent.json",
            ] {
                let method = if path.contains(".json") {
                    "GET"
                } else {
                    "POST"
                };
                let req = Request::builder()
                    .method(method)
                    .uri(format!("/agents/{name}{path}?q=1"))
                    .header("x-test", "preserved")
                    .body(Body::empty())
                    .unwrap();
                let response = app.clone().oneshot(req).await.unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                let body = response.into_body().collect().await.unwrap().to_bytes();
                let relative = if path.is_empty() { "/" } else { path };
                assert_eq!(body, format!("{agent}:{relative}?q=1"));
            }
        }
        for uri in [
            "/agents/unknown/.well-known/agent-card.json",
            "/agents/tools/unknown-path",
        ] {
            assert_eq!(
                app.clone().oneshot(request(uri)).await.unwrap().status(),
                StatusCode::NOT_FOUND
            );
        }
    }

    async fn assert_cards(app: Router, exports: &[Export], headers: &[(&str, &str)], base: &str) {
        for export in exports {
            let mut first = None;
            for key in &export.lookup_keys {
                let name = key.replace('/', "%2F");
                for file in ["agent-card.json", "agent.json"] {
                    let mut request =
                        Request::builder().uri(format!("/agents/{name}/.well-known/{file}"));
                    for (header, value) in headers {
                        request = request.header(*header, *value);
                    }
                    // No user-id or A2A-Version header: cards bypass RPC checks.
                    let response = app
                        .clone()
                        .oneshot(request.body(Body::empty()).unwrap())
                        .await
                        .unwrap();
                    assert_eq!(response.status(), StatusCode::OK, "{name}/{file}");
                    assert_eq!(response.headers()["content-type"], "application/json");
                    let body = response.into_body().collect().await.unwrap().to_bytes();
                    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    assert_eq!(json["name"], export.card_meta.name);
                    assert_eq!(json["description"], export.card_meta.description);
                    assert_eq!(json["version"], export.card_meta.version);
                    assert_eq!(
                        json["supportedInterfaces"],
                        serde_json::json!([{
                            "url": format!("{base}/agents/{}", export.public_name),
                            "protocolBinding": "JSONRPC",
                            "protocolVersion": "1.0"
                        }])
                    );
                    assert_eq!(
                        json["skills"][0]["description"],
                        export.card_meta.description
                    );
                    assert_eq!(
                        json["capabilities"],
                        serde_json::json!({
                            "streaming": true, "pushNotifications": false, "extendedAgentCard": false,
                        })
                    );
                    assert_eq!(
                        json["defaultInputModes"],
                        serde_json::json!(["text/plain", "application/json"])
                    );
                    assert_eq!(
                        json["defaultOutputModes"],
                        serde_json::json!(["text/plain"])
                    );
                    let decoded: a2a_lf::AgentCard =
                        a2a_pb::protojson_conv::from_value(json.clone()).unwrap();
                    assert_eq!(decoded.skills.len(), 1);
                    assert_eq!(
                        decoded.skills[0].examples.clone().unwrap_or_default(),
                        export.card_meta.conversation_starters
                    );
                    if export.public_name == "tools" && base == "https://harnx.example.com" {
                        let golden: serde_json::Value =
                            serde_json::from_str(include_str!("../tests/fixtures/agent_card.json"))
                                .unwrap();
                        assert_eq!(json, golden);
                    }
                    if let Some(first) = &first {
                        assert_eq!(&json, first, "{name}/{file}");
                    } else {
                        first = Some(json);
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn agent_cards_match_for_both_paths_and_all_export_lookup_keys() {
        let sandbox = TestConfigSandbox::new();
        let exports = exports(&sandbox).await;
        let app = router(&exports, None, &[]).unwrap();
        assert_cards(
            app.clone(),
            &exports,
            &[("host", "localhost:3020")],
            "http://localhost:3020",
        )
        .await;
        assert_cards(
            app.clone(),
            &exports,
            &[
                ("host", "harnx.example.com"),
                ("x-forwarded-proto", "https"),
            ],
            "https://harnx.example.com",
        )
        .await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/agents/unknown/.well-known/agent-card.json")
                    .header("host", "localhost:3020")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn agent_cards_use_public_base_url_flag_over_request_headers() {
        use clap::Parser;
        let sandbox = TestConfigSandbox::new();
        let exports = exports(&sandbox).await;
        let args = crate::cli::Args::try_parse_from([
            "harnx-a2a-server",
            "--agent",
            "tools=pkg/agent",
            "--agent",
            "a__b",
            "--public-base-url",
            "https://Harnx.Example.com/prefix/",
            "--user-id-header",
            "x-user-id",
        ])
        .unwrap();
        let app = router(
            &exports,
            args.public_base_url.as_deref(),
            &args.user_id_header,
        )
        .unwrap();
        assert_cards(
            app.clone(),
            &exports,
            &[
                ("host", "internal:3020"),
                ("x-forwarded-host", "ignored.example"),
                ("x-forwarded-proto", "http"),
            ],
            "https://harnx.example.com/prefix",
        )
        .await;
        // A configured URL needs no request host at all.
        assert_cards(app, &exports, &[], "https://harnx.example.com/prefix").await;
    }

    #[tokio::test]
    async fn agent_cards_infer_forwarded_base_url_like_web_serve() {
        let sandbox = TestConfigSandbox::new();
        let exports = exports(&sandbox).await;
        let app = router(&exports, None, &[]).unwrap();
        for (headers, base) in [
            (
                vec![
                    ("host", "internal:3020"),
                    ("x-forwarded-host", " public.example:8443 , proxy.internal"),
                    ("x-forwarded-proto", " HTTPS , http"),
                ],
                "https://public.example:8443",
            ),
            (
                vec![("host", "public.example"), ("x-forwarded-proto", "https")],
                "https://public.example",
            ),
            (
                vec![
                    ("host", "public.example"),
                    ("x-forwarded-host", " , ignored.example"),
                    ("x-forwarded-proto", "ftp"),
                ],
                "http://public.example",
            ),
        ] {
            assert_cards(app.clone(), &exports, &headers, base).await;
        }
    }

    #[tokio::test]
    async fn agent_card_missing_or_invalid_base_returns_bad_request() {
        let sandbox = TestConfigSandbox::new();
        let exports = exports(&sandbox).await;
        assert!(router(&exports, Some("ftp://example.com"), &[]).is_err());
        let app = router(&exports, None, &[]).unwrap();
        for host in [None, Some("user@example.com"), Some("example.com/path")] {
            let mut request = Request::builder().uri("/agents/tools/.well-known/agent-card.json");
            if let Some(host) = host {
                request = request.header("host", host);
            }
            let response = app
                .clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
    }

    #[tokio::test]
    async fn agent_cards_without_alias_use_sanitized_public_name() {
        let sandbox = TestConfigSandbox::new();
        let mut exports = exports(&sandbox).await;
        // Resolve again without an alias, using the same AgentConfig metadata.
        exports[0] = resolve_exports(
            &[AgentSpec {
                name: "pkg/agent".into(),
                alias: None,
            }],
            None,
            Some(sandbox.config_dir()),
        )
        .await
        .unwrap()
        .remove(0);
        assert_eq!(exports[0].public_name, "pkg__agent");
        assert_cards(
            router(&exports, None, &[]).unwrap(),
            &exports,
            &[("host", "localhost:3020")],
            "http://localhost:3020",
        )
        .await;
    }
}
