use super::{
    agent_scoped_config, ensure_frontend_nats_owner, json_response, json_response_with_status,
    negotiate_agents_route, AgentsRepresentation, AgentsRoute, AppResponse, Server,
};
use anyhow::{bail, Context, Result};
use bytes::Bytes;
use futures_util::{stream, Stream, StreamExt};
use harnx_runtime::config::{Config, LOCAL_CLUSTER_KEY};
use http::{HeaderMap, Method, Response, StatusCode};
use http_body_util::{BodyExt, StreamBody};
use hyper::body::{Body, Frame};
use parking_lot::RwLock;
use serde_json::json;
use std::{convert::Infallible, sync::Arc, time::Duration};
use tokio_stream::wrappers::IntervalStream;

const SESSION_METADATA_REQUEST_MAX_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SessionMetadataRoute {
    Metadata,
    Extension(String),
}

#[derive(Clone, Copy)]
pub(crate) struct AgentSessionRef<'a> {
    pub(crate) agent: &'a str,
    pub(crate) session: &'a str,
}

impl<'a> AgentSessionRef<'a> {
    pub(crate) fn new(agent: &'a str, session: &'a str) -> Self {
        Self { agent, session }
    }
}

impl Server {
    pub(super) async fn handle_session_metadata_route<B>(
        &self,
        req: hyper::Request<B>,
        target: AgentSessionRef<'_>,
        route: SessionMetadataRoute,
    ) -> Result<AppResponse>
    where
        B: Body<Data = Bytes> + Send + Unpin,
        B::Error: std::fmt::Display,
    {
        ensure_frontend_nats_owner().await?;
        let jetstream = self.config.nats_jetstream(LOCAL_CLUSTER_KEY).await?;
        let store =
            harnx_runtime::nats_session_metadata::SessionMetadataStore::ensure(&jetstream, 1)
                .await?;

        match (req.method(), route) {
            (&Method::GET, SessionMetadataRoute::Metadata) => {
                let record = store
                    .get_for_agent(target.session, target.agent)
                    .await?
                    .context("Not Found")?;
                metadata_response(&store, target.session, record).await
            }
            (&Method::PATCH, SessionMetadataRoute::Metadata) => {
                let Some(body) = collect_metadata_request_body(req, "metadata patch").await? else {
                    return metadata_payload_too_large();
                };
                let patch: harnx_runtime::nats_session_metadata::SessionMetadataPatch =
                    serde_json::from_slice(&body).context("invalid session metadata patch")?;
                let record = store
                    .apply_patch_for_agent(target.session, target.agent, patch)
                    .await?;
                metadata_response(&store, target.session, record).await
            }
            (&Method::PUT, SessionMetadataRoute::Extension(namespace)) => {
                let Some(body) = collect_metadata_request_body(req, "extension body").await? else {
                    return metadata_payload_too_large();
                };
                let value: serde_json::Value =
                    serde_json::from_slice(&body).context("invalid extension JSON")?;
                let record = store
                    .replace_extension_for_agent(
                        target.session,
                        target.agent,
                        harnx_runtime::nats_session_metadata::SessionExtensionUpdate {
                            namespace: &namespace,
                            value,
                        },
                    )
                    .await?;
                metadata_response(&store, target.session, record).await
            }
            (&Method::DELETE, SessionMetadataRoute::Extension(namespace)) => {
                let record = store
                    .delete_extension_for_agent(target.session, target.agent, &namespace)
                    .await?;
                metadata_response(&store, target.session, record).await
            }
            _ => bail!("Method Not Allowed"),
        }
    }

    pub(super) async fn handle_sessions_route(
        &self,
        method: &Method,
        headers: &HeaderMap,
        agent: &str,
    ) -> Result<AppResponse> {
        match negotiate_agents_route(method, headers, AgentsRoute::Sessions)? {
            AgentsRepresentation::Json if *method == Method::POST => {
                self.create_session_json(agent).await
            }
            AgentsRepresentation::Json => self.sessions_json(agent).await,
            AgentsRepresentation::Html
            | AgentsRepresentation::AgUiSse
            | AgentsRepresentation::AgUiRpc => bail!("Not Acceptable"),
        }
    }

    pub(super) async fn handle_session_events_route(
        &self,
        method: &Method,
        headers: &HeaderMap,
        target: AgentSessionRef<'_>,
    ) -> Result<AppResponse> {
        match negotiate_agents_route(method, headers, AgentsRoute::SessionEvents)? {
            AgentsRepresentation::AgUiSse => self.session_events(target).await,
            AgentsRepresentation::Html
            | AgentsRepresentation::Json
            | AgentsRepresentation::AgUiRpc => bail!("Not Acceptable"),
        }
    }

    async fn create_session_json(&self, agent: &str) -> Result<AppResponse> {
        ensure_frontend_nats_owner().await?;
        let scoped = Arc::new(RwLock::new(agent_scoped_config(&self.config, agent)?));
        let session_id = Config::reserve_new_session_id(&scoped).await?;
        json_response_with_status(StatusCode::CREATED, json!({ "session_id": session_id }))
    }

    async fn session_events(&self, target: AgentSessionRef<'_>) -> Result<AppResponse> {
        let event_stream = attach_agent_session(&self.config, target).await?;
        let stream = stream::select(session_updates(event_stream), keep_alive())
            .map(|bytes| Ok::<_, Infallible>(Frame::data(bytes)));

        Ok(Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/event-stream")
            .header("Cache-Control", "no-cache")
            .header("Connection", "keep-alive")
            .body(BodyExt::boxed(StreamBody::new(stream)))?)
    }
}

async fn metadata_response(
    store: &harnx_runtime::nats_session_metadata::SessionMetadataStore,
    session_id: &str,
    record: harnx_runtime::nats_session_metadata::MetadataRecord,
) -> Result<AppResponse> {
    let activity = store.get_activity(session_id).await?;
    let redacted =
        harnx_runtime::nats_session_metadata::RedactedSessionMetadata::new(record, activity);
    json_response(serde_json::to_value(redacted)?)
}

async fn collect_metadata_request_body<B>(
    req: hyper::Request<B>,
    description: &str,
) -> Result<Option<Bytes>>
where
    B: Body<Data = Bytes> + Send + Unpin,
    B::Error: std::fmt::Display,
{
    if req
        .headers()
        .get(http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .is_some_and(|length| length > SESSION_METADATA_REQUEST_MAX_BYTES)
    {
        return Ok(None);
    }

    let mut body = Vec::new();
    let mut stream = req.into_body().into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|error| anyhow::anyhow!("failed to read {description}: {error}"))?;
        if chunk.len() > SESSION_METADATA_REQUEST_MAX_BYTES.saturating_sub(body.len()) {
            return Ok(None);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Some(Bytes::from(body)))
}

fn metadata_payload_too_large() -> Result<AppResponse> {
    json_response_with_status(
        StatusCode::PAYLOAD_TOO_LARGE,
        json!({
            "error": "payload too large",
            "max_bytes": SESSION_METADATA_REQUEST_MAX_BYTES,
        }),
    )
}

pub(crate) fn parse_session_metadata_route(
    path: &str,
) -> Option<(String, String, SessionMetadataRoute)> {
    let suffix = path.strip_prefix("/v1/agents/")?;
    let segments: Vec<_> = suffix
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    match segments.as_slice() {
        [agent, "sessions", session, "metadata"] => Some((
            super::percent_decode(agent),
            super::percent_decode(session),
            SessionMetadataRoute::Metadata,
        )),
        [agent, "sessions", session, "metadata", "extensions", namespace] => Some((
            super::percent_decode(agent),
            super::percent_decode(session),
            SessionMetadataRoute::Extension(super::percent_decode(namespace)),
        )),
        _ => None,
    }
}

pub(crate) async fn attach_agent_session(
    config: &Config,
    target: AgentSessionRef<'_>,
) -> Result<harnx_runtime::nats_event_sink::SessionEventStream> {
    ensure_frontend_nats_owner().await?;
    let client = config.nats_client(LOCAL_CLUSTER_KEY).await?;
    let jetstream = config.nats_jetstream(LOCAL_CLUSTER_KEY).await?;
    let store =
        harnx_runtime::nats_session_metadata::SessionMetadataStore::ensure(&jetstream, 1).await?;
    let incarnation = store
        .get_for_agent(target.session, target.agent)
        .await?
        .context("Not Found")?
        .metadata
        .created_at;
    let event_stream = harnx_runtime::nats_event_sink::SessionEventStream::attach(
        jetstream,
        client,
        target.session,
    )
    .await?;
    let current = store
        .get_for_agent(target.session, target.agent)
        .await?
        .context("Not Found")?;
    anyhow::ensure!(current.metadata.created_at == incarnation, "Not Found");
    Ok(event_stream)
}

pub(crate) async fn canonical_agent_session_exists(
    config: &Config,
    target: AgentSessionRef<'_>,
) -> Result<bool> {
    ensure_frontend_nats_owner().await?;
    let jetstream = config.nats_jetstream(LOCAL_CLUSTER_KEY).await?;
    let store =
        harnx_runtime::nats_session_metadata::SessionMetadataStore::ensure(&jetstream, 1).await?;
    Ok(store
        .get_for_agent(target.session, target.agent)
        .await?
        .is_some())
}

pub(crate) fn session_updates(
    event_stream: harnx_runtime::nats_event_sink::SessionEventStream,
) -> impl Stream<Item = Bytes> {
    stream::unfold(
        (event_stream, None),
        |(mut stream, mut last_notified_seq)| async move {
            loop {
                let envelope = stream.next().await?;
                if stream.should_render(&envelope) && last_notified_seq != Some(envelope.after_seq)
                {
                    last_notified_seq = Some(envelope.after_seq);
                    let data = json!({ "after_seq": envelope.after_seq });
                    let event = Bytes::from(format!("event: session-updated\ndata: {data}\n\n"));
                    return Some((event, (stream, last_notified_seq)));
                }
            }
        },
    )
}

fn keep_alive() -> impl Stream<Item = Bytes> {
    let mut ticker = tokio::time::interval(Duration::from_secs(15));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tokio_stream::StreamExt::map(
        tokio_stream::StreamExt::skip(IntervalStream::new(ticker), 1),
        |_| Bytes::from_static(b": keep-alive\n\n"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{NatsSessionSeed, TestConfigSandbox};
    use http_body_util::Full;

    async fn response_json(response: AppResponse) -> serde_json::Value {
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("response body")
            .to_bytes();
        serde_json::from_slice(&bytes).expect("JSON response")
    }

    async fn test_metadata_store(
        config: &Config,
    ) -> harnx_runtime::nats_session_metadata::SessionMetadataStore {
        let jetstream = config
            .nats_jetstream(LOCAL_CLUSTER_KEY)
            .await
            .expect("local NATS context");
        harnx_runtime::nats_session_metadata::SessionMetadataStore::ensure(&jetstream, 1)
            .await
            .expect("metadata store")
    }

    async fn metadata_mutation_json(
        server: &Server,
        target: AgentSessionRef<'_>,
        mutation: (Method, SessionMetadataRoute, Bytes),
    ) -> serde_json::Value {
        let (method, route, body) = mutation;
        let request = hyper::Request::builder()
            .method(method)
            .body(Full::new(body))
            .expect("metadata mutation request");
        let response = server
            .handle_session_metadata_route(request, target, route)
            .await
            .expect("metadata mutation response");
        response_json(response).await
    }

    fn assert_patch_response(value: &serde_json::Value) {
        assert!(
            value["title"]["value"] == "Updated title"
                && value["title"]["manual"] == true
                && value["variables"]["TOKEN"]["set"] == true
                && value["overrides"]["model"] == "openai:gpt-5"
                && !value.to_string().contains("private-value")
        );
    }

    async fn assert_metadata_mutations_persisted(
        store: &harnx_runtime::nats_session_metadata::SessionMetadataStore,
        session_id: &str,
    ) {
        let stored = store
            .get(session_id)
            .await
            .expect("read stored metadata")
            .expect("stored metadata");
        assert!(
            stored.metadata.variables["TOKEN"] == "private-value"
                && !stored.metadata.extensions.contains_key("example.client")
        );
    }

    async fn persist_redaction_test_metadata(
        store: &harnx_runtime::nats_session_metadata::SessionMetadataStore,
        session_id: &str,
    ) {
        store
            .patch(session_id, |metadata| {
                metadata
                    .variables
                    .insert("TOKEN".to_string(), "secret-value".to_string());
                Ok(())
            })
            .await
            .expect("persist secret variable");
        store
            .replace_extension(
                session_id,
                "example.client",
                serde_json::json!({"visible": "client-state"}),
            )
            .await
            .expect("persist client-visible extension");
    }

    #[test]
    fn metadata_routes_decode_identity_and_namespace() {
        assert_eq!(
            parse_session_metadata_route("/v1/agents/coding%2Fcoder/sessions/thread-1/metadata"),
            Some((
                "coding/coder".to_string(),
                "thread-1".to_string(),
                SessionMetadataRoute::Metadata,
            ))
        );
        assert_eq!(
            parse_session_metadata_route(
                "/v1/agents/metis/sessions/thread-1/metadata/extensions/example%2Eclient"
            ),
            Some((
                "metis".to_string(),
                "thread-1".to_string(),
                SessionMetadataRoute::Extension("example.client".to_string()),
            ))
        );
        assert_eq!(
            parse_session_metadata_route("/v1/agents/metis/sessions/thread-1/metadata/extra"),
            None
        );
    }

    #[tokio::test]
    async fn metadata_http_response_redacts_variables_but_returns_extensions() {
        harnx_core::require_nextest();
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("metadata-redaction", "You are metadata redaction.");
        let config = sandbox.config();
        let session_id = format!("metadata-redaction-{}", uuid::Uuid::new_v4());
        if !crate::test_support::seed_nats_session(
            &config,
            NatsSessionSeed {
                agent: "metadata-redaction",
                session_id: &session_id,
                messages: &[],
            },
        )
        .await
        {
            return;
        }

        let metadata_store = test_metadata_store(&config).await;
        persist_redaction_test_metadata(&metadata_store, &session_id).await;

        let global = Arc::new(RwLock::new(config));
        let server = Server::new(&global, std::path::PathBuf::from("web-assets"));
        let request = hyper::Request::builder()
            .method(Method::GET)
            .uri(format!(
                "/v1/agents/metadata-redaction/sessions/{session_id}/metadata"
            ))
            .body(Full::new(Bytes::new()))
            .expect("metadata request");
        let response = server
            .handle_session_metadata_route(
                request,
                AgentSessionRef::new("metadata-redaction", &session_id),
                SessionMetadataRoute::Metadata,
            )
            .await
            .expect("metadata response");
        let value = response_json(response).await;
        let encoded = value.to_string();

        assert!(
            value["agent"]["name"] == "metadata-redaction"
                && value["variables"]["TOKEN"]["set"] == true
                && value["extensions"]["example.client"]["visible"] == "client-state"
                && value["revision"].as_u64().is_some()
                && !encoded.contains("secret-value")
        );
    }

    #[tokio::test]
    async fn metadata_http_mutation_routes_patch_put_and_delete() {
        harnx_core::require_nextest();
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("metadata-mutations", "You manage metadata mutations.");
        let config = sandbox.config();
        let session_id = format!("metadata-mutations-{}", uuid::Uuid::new_v4());
        if !crate::test_support::seed_nats_session(
            &config,
            NatsSessionSeed {
                agent: "metadata-mutations",
                session_id: &session_id,
                messages: &[],
            },
        )
        .await
        {
            return;
        }

        let store = test_metadata_store(&config).await;
        let global = Arc::new(RwLock::new(config));
        let server = Server::new(&global, std::path::PathBuf::from("web-assets"));
        let target = AgentSessionRef::new("metadata-mutations", &session_id);

        let patched = metadata_mutation_json(
            &server,
            target,
            (
                Method::PATCH,
                SessionMetadataRoute::Metadata,
                Bytes::from(
                    serde_json::json!({
                        "title": {"value": "Updated title", "manual": true},
                        "variables": {"TOKEN": "private-value"},
                        "overrides": {"model": "openai:gpt-5"}
                    })
                    .to_string(),
                ),
            ),
        )
        .await;
        assert_patch_response(&patched);

        let replaced = metadata_mutation_json(
            &server,
            target,
            (
                Method::PUT,
                SessionMetadataRoute::Extension("example.client".to_string()),
                Bytes::from_static(b"{\"cursor\":7}"),
            ),
        )
        .await;
        assert_eq!(replaced["extensions"]["example.client"]["cursor"], 7);

        let deleted = metadata_mutation_json(
            &server,
            target,
            (
                Method::DELETE,
                SessionMetadataRoute::Extension("example.client".to_string()),
                Bytes::new(),
            ),
        )
        .await;
        assert!(deleted["extensions"].get("example.client").is_none());
        assert_metadata_mutations_persisted(&store, &session_id).await;
    }

    #[tokio::test]
    async fn metadata_http_mutations_reject_invalid_or_mismatched_requests() {
        harnx_core::require_nextest();
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("metadata-validation", "You validate metadata requests.");
        let config = sandbox.config();
        let session_id = format!("metadata-validation-{}", uuid::Uuid::new_v4());
        if !crate::test_support::seed_nats_session(
            &config,
            NatsSessionSeed {
                agent: "metadata-validation",
                session_id: &session_id,
                messages: &[],
            },
        )
        .await
        {
            return;
        }

        let global = Arc::new(RwLock::new(config));
        let server = Server::new(&global, std::path::PathBuf::from("web-assets"));

        let invalid = hyper::Request::builder()
            .method(Method::PATCH)
            .body(Full::new(Bytes::from_static(b"{")))
            .expect("invalid patch request");
        let error = server
            .handle_session_metadata_route(
                invalid,
                AgentSessionRef::new("metadata-validation", &session_id),
                SessionMetadataRoute::Metadata,
            )
            .await
            .expect_err("invalid patch JSON must be rejected");
        assert!(error.to_string().contains("invalid session metadata patch"));

        let mismatched = hyper::Request::builder()
            .method(Method::PATCH)
            .body(Full::new(Bytes::from_static(b"{}")))
            .expect("agent mismatch request");
        let error = server
            .handle_session_metadata_route(
                mismatched,
                AgentSessionRef::new("different-agent", &session_id),
                SessionMetadataRoute::Metadata,
            )
            .await
            .expect_err("agent mismatch must be hidden");
        assert_eq!(error.to_string(), "Not Found");

        let oversized = hyper::Request::builder()
            .method(Method::PUT)
            .header(
                http::header::CONTENT_LENGTH,
                SESSION_METADATA_REQUEST_MAX_BYTES + 1,
            )
            .body(Full::new(Bytes::from_static(b"{}")))
            .expect("oversized extension request");
        let response = server
            .handle_session_metadata_route(
                oversized,
                AgentSessionRef::new("metadata-validation", &session_id),
                SessionMetadataRoute::Extension("example.client".to_string()),
            )
            .await
            .expect("oversized request response");
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn metadata_request_body_is_bounded() {
        let within_limit = hyper::Request::builder()
            .body(Full::new(Bytes::from_static(b"{}")))
            .expect("bounded request");
        assert_eq!(
            collect_metadata_request_body(within_limit, "test body")
                .await
                .expect("read body")
                .expect("body within limit"),
            Bytes::from_static(b"{}")
        );

        let oversized_header = hyper::Request::builder()
            .header(
                http::header::CONTENT_LENGTH,
                SESSION_METADATA_REQUEST_MAX_BYTES + 1,
            )
            .body(Full::new(Bytes::from_static(b"{}")))
            .expect("oversized-header request");
        assert!(collect_metadata_request_body(oversized_header, "test body")
            .await
            .expect("read body")
            .is_none());

        let oversized_body = hyper::Request::builder()
            .body(Full::new(Bytes::from(vec![
                0;
                SESSION_METADATA_REQUEST_MAX_BYTES
                    + 1
            ])))
            .expect("oversized-body request");
        assert!(collect_metadata_request_body(oversized_body, "test body")
            .await
            .expect("read body")
            .is_none());
    }

    #[tokio::test]
    async fn missing_canonical_history_metadata_maps_to_not_found() {
        harnx_core::require_nextest();
        let sandbox = TestConfigSandbox::new();
        let config = sandbox.config();
        ensure_frontend_nats_owner()
            .await
            .expect("local NATS owner");
        let jetstream = config
            .nats_jetstream(LOCAL_CLUSTER_KEY)
            .await
            .expect("jetstream");
        harnx_runtime::nats_session_metadata::SessionMetadataStore::ensure(&jetstream, 1)
            .await
            .expect("metadata bucket");

        let error = crate::load_nats_session(&config, "missing-canonical-metadata")
            .await
            .expect_err("history without canonical metadata must be hidden");
        assert_eq!(error.to_string(), "Not Found");
        assert_eq!(
            crate::status_from_error(&error),
            Some(StatusCode::NOT_FOUND)
        );
    }
}
