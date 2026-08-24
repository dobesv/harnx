use super::{
    agent_scoped_config, ensure_frontend_nats_owner, json_response_with_status,
    negotiate_agents_route, AgentsRepresentation, AgentsRoute, AppResponse, Server,
};
use anyhow::{bail, Result};
use bytes::Bytes;
use futures_util::{stream, Stream, StreamExt};
use harnx_runtime::config::{Config, LOCAL_CLUSTER_KEY};
use http::{HeaderMap, Method, Response, StatusCode};
use http_body_util::{BodyExt, StreamBody};
use hyper::body::Frame;
use parking_lot::RwLock;
use serde_json::json;
use std::{convert::Infallible, sync::Arc, time::Duration, time::SystemTime};
use tokio_stream::wrappers::IntervalStream;

#[derive(Clone, Copy)]
pub(crate) struct AgentSessionRef<'a> {
    pub(crate) agent: &'a str,
    pub(crate) session: &'a str,
}

impl Server {
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

pub(crate) async fn attach_agent_session(
    config: &Config,
    target: AgentSessionRef<'_>,
) -> Result<harnx_runtime::nats_event_sink::SessionEventStream> {
    ensure_frontend_nats_owner().await?;
    let client = config.nats_client(LOCAL_CLUSTER_KEY).await?;
    let jetstream = config.nats_jetstream(LOCAL_CLUSTER_KEY).await?;
    let event_stream = harnx_runtime::nats_event_sink::SessionEventStream::attach(
        jetstream,
        client,
        target.session,
    )
    .await?;
    if event_stream.history().is_empty() {
        // Session creation reserves only an index record; the first prompt
        // creates the canonical log header. Accept that matching provisional
        // identity so a newly-created URL can subscribe before its first turn.
        if indexed_agent_session_exists(config, target).await? {
            return Ok(event_stream);
        }
        bail!("Not Found");
    }
    let loaded = harnx_runtime::nats_session_log::load_session_from_entries(
        event_stream.history(),
        target.session,
    )?;
    if loaded.agent_name.as_deref() != Some(target.agent) {
        bail!("Not Found");
    }
    repair_legacy_session_index_identity(config, target.session, &loaded).await;
    Ok(event_stream)
}

pub(crate) async fn indexed_agent_session_exists(
    config: &Config,
    target: AgentSessionRef<'_>,
) -> Result<bool> {
    ensure_frontend_nats_owner().await?;
    let jetstream = config.nats_jetstream(LOCAL_CLUSTER_KEY).await?;
    let store = harnx_runtime::nats_session_index::ensure_index_bucket(&jetstream, 1).await?;
    Ok(
        harnx_runtime::nats_session_index::get_record(&store, target.session)
            .await?
            .is_some_and(|record| record.agent_name == target.agent),
    )
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

pub(super) async fn repair_legacy_session_index_identity(
    config: &Config,
    stream_session_id: &str,
    session: &harnx_core::session::Session,
) {
    let Some((stale_id, agent_name)) = stale_index_identity(stream_session_id, session) else {
        return;
    };
    let repair = async {
        let jetstream = config.nats_jetstream(LOCAL_CLUSTER_KEY).await?;
        let store = harnx_runtime::nats_session_index::ensure_index_bucket(&jetstream, 1).await?;
        harnx_runtime::nats_session_index::put_record(
            &store,
            &harnx_runtime::nats_session_index::SessionIndexRecord {
                session_id: stream_session_id.to_string(),
                agent_name,
                working_dir: session.working_dir.clone(),
                git_branch: session.git_branch.clone(),
                git_remote: session.git_remote.clone(),
                title: session.title().map(ToOwned::to_owned),
                last_activity: SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            },
        )
        .await?;
        harnx_runtime::nats_session_index::delete_record(&store, &stale_id).await
    };
    if let Err(error) = repair.await {
        log::warn!(
            "failed to repair legacy session index identity: stale_id={stale_id} \
             session_id={stream_session_id} error={error:#}"
        );
    }
}

fn stale_index_identity(
    stream_session_id: &str,
    session: &harnx_core::session::Session,
) -> Option<(String, String)> {
    let stale_id = session
        .session_id
        .as_ref()
        .filter(|id| id.as_str() != stream_session_id)?;
    Some((stale_id.clone(), session.agent_name.clone()?))
}
