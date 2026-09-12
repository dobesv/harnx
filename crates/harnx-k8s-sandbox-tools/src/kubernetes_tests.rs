use super::*;
use http::{Method, Request, Response, StatusCode};
use http_body_util::BodyExt;
use kube::client::Body;
use parking_lot::Mutex;
use std::convert::Infallible;
use std::sync::Arc;
use tokio::sync::Notify;
use tower::service_fn;

#[derive(Clone, Debug)]
struct RecordedRequest {
    method: Method,
    path: String,
    body: Option<Value>,
}

type Responder = dyn Fn(&RecordedRequest) -> (StatusCode, Value) + Send + Sync;

fn test_api(
    responder: impl Fn(&RecordedRequest) -> (StatusCode, Value) + Send + Sync + 'static,
) -> (KubernetesSandboxApi, Arc<Mutex<Vec<RecordedRequest>>>) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let responder: Arc<Responder> = Arc::new(responder);
    let service = service_fn({
        let requests = requests.clone();
        move |request: Request<Body>| {
            let requests = requests.clone();
            let responder = responder.clone();
            async move {
                let (parts, body) = request.into_parts();
                let bytes = body.collect().await.unwrap().to_bytes();
                let request = RecordedRequest {
                    method: parts.method,
                    path: parts.uri.path().to_string(),
                    body: (!bytes.is_empty()).then(|| serde_json::from_slice(&bytes).unwrap()),
                };
                let (status, response) = responder(&request);
                requests.lock().push(request);
                Ok::<_, Infallible>(
                    Response::builder()
                        .status(status)
                        .header("content-type", "application/json")
                        .body(Body::from(serde_json::to_vec(&response).unwrap()))
                        .unwrap(),
                )
            }
        }
    });
    (
        KubernetesSandboxApi::new(Client::new(service, "test-ns"), "test-ns"),
        requests,
    )
}

fn claim(name: &str, sandbox_name: Option<&str>, last_activity: Option<&str>) -> Value {
    let mut annotations = serde_json::Map::new();
    if let Some(last_activity) = last_activity {
        annotations.insert(
            LAST_ACTIVITY_ANNOTATION.to_string(),
            Value::String(last_activity.to_string()),
        );
    }
    json!({
        "apiVersion": "extensions.agents.x-k8s.io/v1alpha1",
        "kind": "SandboxClaim",
        "metadata": {
            "name": name,
            "namespace": "test-ns",
            "creationTimestamp": "2026-09-09T00:00:00Z",
            "annotations": annotations,
        },
        "spec": {
            "sandboxTemplateRef": {"name": "formative-buildbox"},
            "lifecycle": {
                "shutdownTime": "2026-09-12T00:00:00Z",
                "shutdownPolicy": "Delete"
            }
        },
        "status": {
            "conditions": [{
                "type": "Ready",
                "status": "True",
                "reason": "Ready",
                "message": "ready"
            }],
            "sandbox": sandbox_name.map(|name| json!({
                "name": name,
                "podIPs": ["10.0.0.42"]
            }))
        }
    })
}

fn sandbox(name: &str, replicas: i64) -> Value {
    json!({
        "apiVersion": "agents.x-k8s.io/v1alpha1",
        "kind": "Sandbox",
        "metadata": {"name": name, "namespace": "test-ns"},
        "spec": {"replicas": replicas}
    })
}

fn failure(status: StatusCode, message: &str) -> (StatusCode, Value) {
    let reason = match status {
        StatusCode::NOT_FOUND => "NotFound",
        StatusCode::CONFLICT => "AlreadyExists",
        StatusCode::INTERNAL_SERVER_ERROR => "InternalError",
        _ => "Error",
    };
    (
        status,
        json!({
            "apiVersion": "v1",
            "kind": "Status",
            "status": "Failure",
            "message": message,
            "reason": reason,
            "code": status.as_u16()
        }),
    )
}

fn assert_create_request(request: &RecordedRequest) {
    let body = request.body.as_ref().unwrap();
    assert_eq!(
        (
            &request.method,
            request.path.as_str(),
            &body["metadata"]["name"],
            &body["metadata"]["annotations"]["kubernetes.io/description"],
            &body["spec"]["sandboxTemplateRef"]["name"],
            &body["spec"]["lifecycle"]["shutdownPolicy"],
            &body["spec"]["lifecycle"]["shutdownTime"],
        ),
        (
            &Method::POST,
            "/apis/extensions.agents.x-k8s.io/v1alpha1/namespaces/test-ns/sandboxclaims",
            &json!("claim-a"),
            &json!("review change"),
            &json!("formative-buildbox"),
            &json!("Delete"),
            &json!("2026-09-12T00:00:00+00:00"),
        )
    );
}

fn sandbox_api_response(request: &RecordedRequest) -> (StatusCode, Value) {
    match (request.method.clone(), request.path.as_str()) {
        (Method::GET, path) if path.ends_with("/sandboxclaims/claim-a") => {
            (StatusCode::OK, claim("claim-a", Some("sandbox-a"), None))
        }
        (Method::GET, path) if path.ends_with("/sandboxes/sandbox-a") => {
            (StatusCode::OK, sandbox("sandbox-a", 1))
        }
        (Method::PATCH, path) if path.ends_with("/sandboxes/sandbox-a") => {
            (StatusCode::OK, sandbox("sandbox-a", 0))
        }
        (Method::PATCH, path) if path.ends_with("/sandboxclaims/claim-a") => {
            (StatusCode::OK, claim("claim-a", Some("sandbox-a"), None))
        }
        _ => panic!("unexpected request: {request:?}"),
    }
}

fn assert_claim_record(record: &SandboxRecord) {
    let created_at = DateTime::parse_from_rfc3339("2026-09-09T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    assert_eq!(
        (
            record.sandbox_name.as_deref(),
            record.replicas,
            record
                .conditions
                .first()
                .map(|condition| condition.kind.as_str()),
            record.created_at.as_ref(),
        ),
        (Some("sandbox-a"), Some(1), Some("Ready"), Some(&created_at))
    );
    assert_eq!(record.pod_ips, ["10.0.0.42"]);
}

fn assert_replicas_patch(requests: &[RecordedRequest]) {
    assert!(requests.iter().any(|request| {
        request.method == Method::PATCH
            && request.path.ends_with("/sandboxes/sandbox-a")
            && request.body.as_ref().unwrap()["spec"]["replicas"] == 0
    }));
}

fn assert_shutdown_patch(requests: &[RecordedRequest]) {
    assert!(requests.iter().any(|request| {
        request.method == Method::PATCH
            && request.path.ends_with("/sandboxclaims/claim-a")
            && request.body.as_ref().unwrap()["spec"]["lifecycle"]["shutdownTime"]
                == "2026-09-13T00:00:00+00:00"
    }));
}

fn assert_activity_patch(requests: &[RecordedRequest]) {
    assert!(requests.iter().any(|request| {
        request.method == Method::PATCH
            && request.path.ends_with("/sandboxclaims/claim-a")
            && request.body.as_ref().unwrap()["metadata"]["annotations"][LAST_ACTIVITY_ANNOTATION]
                == "2026-09-09T01:00:00+00:00"
    }));
}

#[tokio::test]
async fn create_claim_uses_the_agent_sandbox_wire_contract() {
    let (api, requests) = test_api(|request| {
        let mut created = request.body.clone().unwrap();
        created["metadata"]["name"] = json!("claim-a");
        (StatusCode::CREATED, created)
    });
    let shutdown_time = DateTime::parse_from_rfc3339("2026-09-12T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);

    let id = api
        .create_claim(CreateSandboxClaim {
            name: "claim-a".to_string(),
            template: "formative-buildbox".to_string(),
            shutdown_time,
            description: Some("review change".to_string()),
        })
        .await
        .unwrap();

    assert_eq!(id, "claim-a");
    let requests = requests.lock();
    assert_create_request(&requests[0]);
}

#[tokio::test]
async fn create_conflict_and_delete_not_found_are_idempotent() {
    let (api, requests) = test_api(|request| match request.method {
        Method::POST => failure(StatusCode::CONFLICT, "already exists"),
        Method::DELETE => failure(StatusCode::NOT_FOUND, "not found"),
        _ => panic!("unexpected request: {request:?}"),
    });
    let shutdown_time = Utc::now() + chrono::Duration::hours(1);

    assert_eq!(
        api.create_claim(CreateSandboxClaim {
            name: "claim-retry".to_string(),
            template: "template".to_string(),
            shutdown_time,
            description: None,
        })
        .await
        .unwrap(),
        "claim-retry"
    );
    api.delete("claim-retry").await.unwrap();

    let requests = requests.lock();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].method, Method::POST);
    assert_eq!(requests[1].method, Method::DELETE);
}

#[tokio::test]
async fn get_and_patches_use_compatible_status_paths_and_payloads() {
    let (api, requests) = test_api(sandbox_api_response);

    let record = api.get("claim-a").await.unwrap().unwrap();
    assert_claim_record(&record);

    api.set_replicas("claim-a", 0).await.unwrap();
    api.update_shutdown_time(
        "claim-a",
        DateTime::parse_from_rfc3339("2026-09-13T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc),
    )
    .await
    .unwrap();
    api.bump_activity(
        "claim-a",
        DateTime::parse_from_rfc3339("2026-09-09T01:00:00Z")
            .unwrap()
            .with_timezone(&Utc),
    )
    .await
    .unwrap();

    let requests = requests.lock();
    assert_replicas_patch(&requests);
    assert_shutdown_patch(&requests);
    assert_activity_patch(&requests);
}

#[tokio::test]
async fn list_isolates_backing_sandbox_failures_and_ignores_bad_activity_timestamps() {
    let (api, _) = test_api(
        |request| match (request.method.clone(), request.path.as_str()) {
            (Method::GET, path) if path.ends_with("/sandboxclaims") => (
                StatusCode::OK,
                json!({
                    "apiVersion": "extensions.agents.x-k8s.io/v1alpha1",
                    "kind": "SandboxClaimList",
                    "metadata": {"resourceVersion": "1"},
                    "items": [
                        claim("bad", Some("sandbox-bad"), None),
                        claim("good", Some("sandbox-good"), Some("not-a-timestamp"))
                    ]
                }),
            ),
            (Method::GET, path) if path.ends_with("/sandboxes/sandbox-bad") => {
                failure(StatusCode::INTERNAL_SERVER_ERROR, "temporarily unavailable")
            }
            (Method::GET, path) if path.ends_with("/sandboxes/sandbox-good") => {
                (StatusCode::OK, sandbox("sandbox-good", 1))
            }
            _ => panic!("unexpected request: {request:?}"),
        },
    );

    let records = api.list().await.unwrap();

    assert_eq!(records.len(), 1);
    assert_eq!(records[0].id, "good");
    assert_eq!(records[0].replicas, Some(1));
    assert_eq!(records[0].last_activity, None);
}

#[tokio::test]
async fn activity_updates_are_debounced_and_invalid_annotations_are_repaired() {
    let now = DateTime::parse_from_rfc3339("2026-09-09T01:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let (recent, recent_requests) = test_api(|request| {
        assert_eq!(request.method, Method::GET);
        (
            StatusCode::OK,
            claim(
                "claim-recent",
                Some("sandbox-recent"),
                Some("2026-09-09T00:59:30Z"),
            ),
        )
    });
    recent.bump_activity("claim-recent", now).await.unwrap();
    assert_eq!(recent_requests.lock().len(), 1);

    let (invalid, invalid_requests) = test_api(|request| match request.method {
        Method::GET => (
            StatusCode::OK,
            claim("claim-invalid", Some("sandbox-invalid"), Some("invalid")),
        ),
        Method::PATCH => (
            StatusCode::OK,
            claim("claim-invalid", Some("sandbox-invalid"), None),
        ),
        _ => panic!("unexpected request: {request:?}"),
    });
    invalid.bump_activity("claim-invalid", now).await.unwrap();
    let requests = invalid_requests.lock();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].method, Method::GET);
    assert_eq!(requests[1].method, Method::PATCH);
    assert_eq!(
        requests[1].body.as_ref().unwrap()["metadata"]["annotations"][LAST_ACTIVITY_ANNOTATION],
        "2026-09-09T01:00:00+00:00"
    );
}

#[tokio::test]
async fn get_maps_a_missing_claim_to_none() {
    let (api, _) = test_api(|request| {
        assert_eq!(request.method, Method::GET);
        failure(StatusCode::NOT_FOUND, "not found")
    });

    assert!(api.get("missing").await.unwrap().is_none());
}

#[tokio::test(start_paused = true)]
async fn hung_kubernetes_request_hits_per_request_timeout() {
    let started = Arc::new(Notify::new());
    let waiting = started.notified();
    let service = service_fn({
        let started = started.clone();
        move |_request: Request<Body>| {
            let started = started.clone();
            async move {
                started.notify_one();
                std::future::pending::<Result<Response<Body>, Infallible>>().await
            }
        }
    });
    let api = KubernetesSandboxApi::with_timeouts(
        Client::new(service, "test-ns"),
        "test-ns",
        Duration::from_secs(5),
        Duration::from_secs(20),
    );
    let call = tokio::spawn(async move { api.get("hung").await });
    waiting.await;
    tokio::time::advance(Duration::from_secs(5)).await;

    let error = call.await.unwrap().unwrap_err();
    assert_eq!(kubernetes_failure(&error).0, FailureKind::Timeout);
}

#[tokio::test(start_paused = true)]
async fn composite_deadline_bounds_multiple_kubernetes_requests() {
    let second_started = Arc::new(Notify::new());
    let waiting = second_started.notified();
    let service = service_fn({
        let second_started = second_started.clone();
        move |request: Request<Body>| {
            let second_started = second_started.clone();
            async move {
                let path = request.uri().path().to_string();
                let value = if path.ends_with("/sandboxclaims/aggregate") {
                    claim("aggregate", Some("sandbox-aggregate"), None)
                } else {
                    second_started.notify_one();
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    sandbox("sandbox-aggregate", 1)
                };
                Ok::<_, Infallible>(
                    Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "application/json")
                        .body(Body::from(serde_json::to_vec(&value).unwrap()))
                        .unwrap(),
                )
            }
        }
    });
    let api = KubernetesSandboxApi::with_timeouts(
        Client::new(service, "test-ns"),
        "test-ns",
        Duration::from_secs(20),
        Duration::from_secs(5),
    );
    let call = tokio::spawn(async move { api.get("aggregate").await });
    waiting.await;
    tokio::time::advance(Duration::from_secs(5)).await;

    let error = call.await.unwrap().unwrap_err();
    assert_eq!(kubernetes_failure(&error).0, FailureKind::Timeout);
}

#[test]
fn kubernetes_status_classification_distinguishes_permanent_and_transient() {
    let status_error = |code| {
        kube::Error::Api(
            kube::error::Status::failure("scripted", "scripted")
                .with_code(code)
                .boxed(),
        )
    };

    for code in [400, 401, 403, 404, 422] {
        assert_eq!(
            classify_kube_error(&status_error(code)),
            FailureKind::Permanent
        );
    }
    for code in [408, 409, 429, 500, 503] {
        assert_eq!(
            classify_kube_error(&status_error(code)),
            FailureKind::RemoteTransient
        );
    }
}

#[test]
fn service_error_classification_inspects_typed_source_chain() {
    let service_error = |kind| kube::Error::Service(Box::new(std::io::Error::from(kind)));

    assert_eq!(
        classify_kube_error(&service_error(std::io::ErrorKind::ConnectionReset)),
        FailureKind::Transport
    );
    assert_eq!(
        classify_kube_error(&service_error(std::io::ErrorKind::TimedOut)),
        FailureKind::Timeout
    );
    assert_eq!(
        classify_kube_error(&service_error(std::io::ErrorKind::PermissionDenied)),
        FailureKind::Permanent
    );
}

async fn empty_http_response(
    _request: Request<hyper::body::Incoming>,
) -> Result<Response<http_body_util::Empty<hyper::body::Bytes>>, Infallible> {
    Ok(Response::new(http_body_util::Empty::new()))
}

#[tokio::test(start_paused = true)]
async fn hyper_header_timeout_is_classified_as_timeout() {
    let (_client, server) = tokio::io::duplex(64);
    let mut builder = hyper::server::conn::http1::Builder::new();
    builder
        .timer(hyper_util::rt::TokioTimer::new())
        .header_read_timeout(Duration::from_secs(5));
    let connection = builder.serve_connection(
        hyper_util::rt::TokioIo::new(server),
        hyper::service::service_fn(empty_http_response),
    );
    tokio::pin!(connection);

    tokio::time::advance(Duration::from_secs(5)).await;
    let error = connection.await.unwrap_err();

    assert!(error.is_timeout());
    assert_eq!(classify_hyper_error(&error), FailureKind::Timeout);
}

#[tokio::test]
async fn incomplete_hyper_message_is_classified_as_broken_transport() {
    use tokio::io::AsyncWriteExt as _;

    let (mut client, server) = tokio::io::duplex(64);
    let mut builder = hyper::server::conn::http1::Builder::new();
    builder
        .timer(hyper_util::rt::TokioTimer::new())
        .header_read_timeout(None);
    let connection = builder.serve_connection(
        hyper_util::rt::TokioIo::new(server),
        hyper::service::service_fn(empty_http_response),
    );
    client
        .write_all(b"GET / HTTP/1.1\r\nHost: sandbox")
        .await
        .unwrap();
    drop(client);

    let error = connection.await.unwrap_err();

    assert!(error.is_incomplete_message());
    assert!(is_broken_transport(&error));
    assert_eq!(classify_hyper_error(&error), FailureKind::Transport);
}
