//! Session properties against a real broker: the reserved namespace, worker
//! fencing, which Web UI address wins, and the session metadata tools
//! writing under a worker's lease.

use crate::common;

use anyhow::{Context, Result};
use common::{spawn_nats_server, NatsServerHandle};
use harnx_core::require_nextest;
use harnx_core::session::Session;
use harnx_core::tool::{ToolError, ToolProvider};
use harnx_runtime::config::session::SessionAppendSink;
use harnx_runtime::config::{Config, ConfigLock};
use harnx_runtime::nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease};
use harnx_runtime::nats_session_metadata::{
    session_properties, PropertySource, SessionInitializer, SessionMetadata, SessionMetadataPatch,
    SessionMetadataStore, SessionPropertiesUpdate, SessionTitlePatch, SESSION_PROPERTIES_NAMESPACE,
};
use harnx_runtime::nats_worker::{FencedSessionLogSink, NatsSessionLogBackend};
use harnx_runtime::session_meta_tool::{SessionMetaProvider, READ_TOOL_NAME, WRITE_TOOL_NAME};
use serde_json::{json, Value};
use std::sync::Arc;

/// A broker holding one fresh session of agent `metis`.
struct Broker {
    _server: NatsServerHandle,
    jetstream: async_nats::jetstream::Context,
    store: SessionMetadataStore,
    local_id: String,
    session_id: String,
}

async fn broker() -> Result<Option<Broker>> {
    broker_with_properties(json!({})).await
}

async fn broker_with_properties(properties: Value) -> Result<Option<Broker>> {
    let Some(server) = spawn_nats_server().await? else {
        return Ok(None);
    };
    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client);
    let store = SessionMetadataStore::ensure(&jetstream, 1).await?;
    let local_id = format!("properties-{}", uuid::Uuid::new_v4());
    let mut metadata = SessionMetadata::new(
        &local_id,
        SessionInitializer::named("metis", Default::default()),
    );
    metadata
        .extensions
        .insert(SESSION_PROPERTIES_NAMESPACE.to_string(), properties);
    store.create(&metadata).await?;
    Ok(Some(Broker {
        _server: server,
        jetstream,
        store,
        local_id,
        session_id: metadata.storage_key(),
    }))
}

fn update(value: Value) -> SessionPropertiesUpdate {
    serde_json::from_value(value).expect("valid session properties update")
}

impl Broker {
    async fn properties(&self) -> Result<Value> {
        let record = self
            .store
            .get(&self.session_id)
            .await?
            .context("session metadata exists")?;
        Ok(serde_json::to_value(session_properties(&record.metadata)?)?)
    }

    async fn lease(&self) -> Result<Arc<NatsSessionLease>> {
        let lease = NatsSessionLease::acquire(NatsLeaseAcquireParams {
            jetstream: self.jetstream.clone(),
            session_id: &self.session_id,
            worker_id: "worker-a".to_string(),
            generation: 1,
            config: NatsLeaseConfig {
                replicas: 1,
                ..Default::default()
            },
            session_metadata: None,
        })
        .await?
        .context("an unheld session lease is acquired")?;
        Ok(Arc::new(lease))
    }

    /// The session metadata tools acting on this session the way they do in
    /// a worker's turn: through a sink fenced by `lease`.
    fn tools(&self, lease: &Arc<NatsSessionLease>) -> SessionMetaProvider {
        let backend =
            NatsSessionLogBackend::new(self.jetstream.clone(), self.session_id.clone(), 1)
                .with_metadata_store(Some(self.store.clone()));
        let sink: Arc<dyn SessionAppendSink> =
            Arc::new(FencedSessionLogSink::new(backend, Arc::clone(lease)));
        let session = Session {
            id: self.local_id.clone(),
            runtime: Some(Arc::new(sink)),
            ..Default::default()
        };
        SessionMetaProvider::new(Arc::new(ConfigLock::new(Config {
            session: Some(session),
            ..Config::default()
        })))
    }
}

/// The JSON view a tool call returns, or its error as `recoverable: ...` or
/// `fatal: ...`.
async fn call(
    provider: &SessionMetaProvider,
    tool: &str,
    arguments: Value,
) -> std::result::Result<Value, String> {
    let output = provider
        .call_tool(tool, arguments, &harnx_core::abort::create_abort_signal())
        .await
        .map_err(|error| match error {
            ToolError::Recoverable(error) => format!("recoverable: {error:#}"),
            ToolError::Fatal(error) => format!("fatal: {error:#}"),
        })?;
    let text = output.value["content"][0]["text"]
        .as_str()
        .expect("text content");
    Ok(serde_json::from_str(text).expect("JSON view"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn namespace_is_reserved_and_worker_writes_are_fenced() -> Result<()> {
    require_nextest();
    let Some(broker) = broker().await? else {
        return Ok(());
    };
    let store = &broker.store;
    let session_id = &broker.session_id;
    let replaced = store
        .replace_extension(session_id, SESSION_PROPERTIES_NAMESPACE, json!({}))
        .await;
    let deleted = store
        .delete_extension(session_id, SESSION_PROPERTIES_NAMESPACE)
        .await;
    for result in [replaced, deleted] {
        assert!(result.unwrap_err().to_string().contains("reserved"));
    }

    let main = update(json!({"set": [{"name": "git_branch", "value": "main"}]}));
    let stale = update(json!({"set": [{"name": "git_branch", "value": "stale"}]}));
    let written = store
        .update_session_properties(session_id, &main, Some(20))
        .await?;
    let error = store
        .update_session_properties(session_id, &stale, Some(19))
        .await
        .expect_err("an older worker fence must be rejected");
    assert!(error.to_string().contains("stale session metadata writer"));

    let unchanged = store
        .update_session_properties(session_id, &main, Some(20))
        .await?;
    assert_eq!(
        unchanged.revision, written.revision,
        "a change that alters nothing must not write"
    );
    assert_eq!(
        broker.properties().await?,
        json!({"git_branch": {"value": "main", "inherit": true}})
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn web_session_url_is_recorded_alongside_concurrent_writers() -> Result<()> {
    require_nextest();
    let Some(broker) = broker().await? else {
        return Ok(());
    };
    let store = &broker.store;
    let session_id = &broker.session_id;
    let url = format!(
        "https://harnx.example/agents/metis/sessions/{}",
        broker.local_id
    );
    let title = SessionMetadataPatch {
        title: Some(SessionTitlePatch {
            value: Some("Concurrent title".to_string()),
            manual: true,
        }),
        ..Default::default()
    };
    let labels = update(json!({"add_labels": ["bug"]}));
    let (recorded, titled, labelled) = tokio::join!(
        store.record_web_session_url(session_id, &url, PropertySource::Inferred),
        store.apply_patch(session_id, title),
        store.update_session_properties(session_id, &labels, None),
    );
    assert!(recorded?, "the first address must be recorded");
    titled?;
    labelled?;
    assert!(store
        .record_web_session_url(
            session_id,
            "javascript:alert(1)",
            PropertySource::Configured
        )
        .await
        .is_err());

    let record = store.get(session_id).await?.context("metadata exists")?;
    assert_eq!(
        record.metadata.title.value.as_deref(),
        Some("Concurrent title")
    );
    assert_eq!(
        broker.properties().await?,
        json!({
            "web_session_url": {"value": url, "inherit": false, "source": "inferred"},
            "labels": {"value": ["bug"], "inherit": false},
        })
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_address_replaces_what_harnx_recorded_but_not_what_an_agent_set() -> Result<()> {
    require_nextest();
    let Some(broker) = broker().await? else {
        return Ok(());
    };
    let store = &broker.store;
    let session_id = &broker.session_id;
    let record = |url: &'static str, source| store.record_web_session_url(session_id, url, source);
    let address = || async {
        let properties = broker.properties().await?;
        Ok::<_, anyhow::Error>(properties["web_session_url"].clone())
    };

    assert!(record("http://internal:8000/a", PropertySource::Inferred).await?);
    assert!(
        !record("http://other-proxy/a", PropertySource::Inferred).await?,
        "a second inference must not replace the first"
    );
    assert!(record("https://public.example/a", PropertySource::Configured).await?);
    assert!(
        !record("http://internal:8000/a", PropertySource::Inferred).await?,
        "an inference must not replace a configured address"
    );
    assert!(
        record("https://moved.example/a", PropertySource::Configured).await?,
        "a newly configured URL replaces the old one"
    );
    assert_eq!(
        address().await?,
        json!({"value": "https://moved.example/a", "inherit": false, "source": "configured"})
    );

    let chosen = update(json!({"set": [
        {"name": "web_session_url", "value": "https://chosen.example/a"}
    ]}));
    store
        .update_session_properties(session_id, &chosen, None)
        .await?;
    assert!(
        !record("https://public.example/a", PropertySource::Configured).await?,
        "an address an agent set is left alone"
    );
    assert_eq!(
        address().await?,
        json!({"value": "https://chosen.example/a", "inherit": false})
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tools_write_under_the_worker_lease() -> Result<()> {
    require_nextest();
    let Some(broker) = broker().await? else {
        return Ok(());
    };
    let lease = broker.lease().await?;
    let tools = broker.tools(&lease);
    let issue = json!({"set": [{"name": "github_issue", "value": 2296}]});

    let written = call(&tools, WRITE_TOOL_NAME, issue.clone())
        .await
        .expect("write succeeds");
    assert_eq!(written["session_id"], broker.local_id.as_str());
    assert_eq!(
        written["properties"],
        json!({"github_issue": {"value": 2296, "inherit": true}})
    );
    let read = call(&tools, READ_TOOL_NAME, json!({}))
        .await
        .expect("read succeeds");
    assert_eq!(read["properties"], written["properties"]);

    // The write was fenced, so a writer holding no lease revision can no
    // longer change the metadata. Renewals keep raising the lease's own
    // token, which is why this compares against 0 rather than the token the
    // write used.
    let stale = update(json!({"set": [{"name": "git_branch", "value": "stale"}]}));
    let error = broker
        .store
        .update_session_properties(&broker.session_id, &stale, Some(0))
        .await
        .expect_err("the tool's write must advance the metadata fence");
    assert!(error.to_string().contains("stale session metadata writer"));

    lease.release().await?;
    let error = call(&tools, WRITE_TOOL_NAME, issue)
        .await
        .expect_err("a worker that lost its lease must not write");
    assert!(
        error.starts_with("recoverable: ") && error.contains("session lease lost"),
        "{error}"
    );
    assert_eq!(broker.properties().await?, written["properties"]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_address_is_inserted_only_when_absent() -> Result<()> {
    require_nextest();
    let Some(broker) = broker().await? else {
        return Ok(());
    };
    let store = &broker.store;
    let session_id = &broker.session_id;
    for invalid in ["", "javascript:alert(1)"] {
        assert!(store
            .record_web_session_url_if_absent(session_id, invalid)
            .await
            .is_err());
    }
    assert!(broker.properties().await?.as_object().unwrap().is_empty());
    assert!(
        store
            .record_web_session_url_if_absent(session_id, "https://worker.example/a")
            .await?
    );
    assert_eq!(
        broker.properties().await?,
        json!({"web_session_url": {
            "value": "https://worker.example/a",
            "inherit": false,
            "source": "configured",
            "recorded_by": "worker",
        }})
    );
    let before = store.get(session_id).await?.context("metadata exists")?;
    assert!(
        !store
            .record_web_session_url_if_absent(session_id, "https://other-worker.example/a")
            .await?
    );
    let after = store.get(session_id).await?.context("metadata exists")?;
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.metadata, before.metadata);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_address_never_replaces_an_existing_property() -> Result<()> {
    require_nextest();
    for source in [Some("configured"), Some("inferred"), None] {
        for marker in [Some("worker"), None] {
            let mut properties = json!({"web_session_url": {
                "value": "https://existing.example/a",
                "inherit": false,
                "source": source,
                "recorded_by": marker,
            }});
            properties["web_session_url"]
                .as_object_mut()
                .unwrap()
                .retain(|_, value| !value.is_null());
            let Some(broker) = broker_with_properties(properties).await? else {
                return Ok(());
            };
            let store = &broker.store;
            let session_id = &broker.session_id;
            let before = store.get(session_id).await?.context("metadata exists")?;
            assert!(
                !store
                    .record_web_session_url_if_absent(session_id, "https://worker.example/a")
                    .await?
            );
            let after = store.get(session_id).await?.context("metadata exists")?;
            assert_eq!(after.revision, before.revision);
            assert_eq!(after.metadata, before.metadata);
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serve_inferred_address_replaces_a_worker_address() -> Result<()> {
    require_nextest();
    let Some(broker) = broker().await? else {
        return Ok(());
    };
    let store = &broker.store;
    let session_id = &broker.session_id;
    assert!(
        store
            .record_web_session_url_if_absent(session_id, "https://worker.example/a")
            .await?
    );
    assert!(
        store
            .record_web_session_url(
                session_id,
                "https://serve.example/a",
                PropertySource::Inferred
            )
            .await?
    );
    assert_eq!(
        broker.properties().await?,
        json!({"web_session_url": {
            "value": "https://serve.example/a", "inherit": false, "source": "inferred",
        }})
    );
    assert!(
        !store
            .record_web_session_url_if_absent(session_id, "https://worker.example/a")
            .await?
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serve_same_url_takeover_clears_worker_provenance() -> Result<()> {
    require_nextest();
    for source in [PropertySource::Configured, PropertySource::Inferred] {
        let url = "https://shared.example/a";
        let Some(broker) = broker_with_properties(json!({"web_session_url": {
            "value": url,
            "inherit": false,
            "source": "configured",
            "recorded_by": "worker",
            "future_attribute": 42,
        }}))
        .await?
        else {
            return Ok(());
        };
        let store = &broker.store;
        let session_id = &broker.session_id;
        assert!(
            store
                .record_web_session_url(session_id, url, source)
                .await?
        );
        assert_eq!(
            broker.properties().await?,
            json!({"web_session_url": {
                "value": url, "inherit": false, "source": source, "future_attribute": 42,
            }})
        );
        let before = store.get(session_id).await?.context("metadata exists")?;
        assert!(
            !store
                .record_web_session_url(session_id, url, source)
                .await?
        );
        assert!(
            !store
                .record_web_session_url(
                    session_id,
                    "https://other.example/a",
                    PropertySource::Inferred
                )
                .await?
        );
        let after = store.get(session_id).await?.context("metadata exists")?;
        assert_eq!(after.revision, before.revision);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_address_with_stale_worker_marker_is_protected() -> Result<()> {
    require_nextest();
    let properties = json!({"web_session_url": {
        "value": "https://chosen.example/a", "inherit": false, "recorded_by": "worker",
    }});
    let Some(broker) = broker_with_properties(properties.clone()).await? else {
        return Ok(());
    };
    for source in [PropertySource::Inferred, PropertySource::Configured] {
        assert!(
            !broker
                .store
                .record_web_session_url(&broker.session_id, "https://serve.example/a", source)
                .await?
        );
    }
    assert_eq!(broker.properties().await?, properties);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_worker_and_serve_addresses_leave_serve_authoritative() -> Result<()> {
    require_nextest();
    for source in [PropertySource::Inferred, PropertySource::Configured] {
        let Some(broker) = broker().await? else {
            return Ok(());
        };
        let store = &broker.store;
        let session_id = &broker.session_id;
        let (worker, serve) = tokio::join!(
            store.record_web_session_url_if_absent(session_id, "https://worker.example/a"),
            store.record_web_session_url(session_id, "https://serve.example/a", source),
        );
        worker?;
        assert!(serve?);
        assert_eq!(
            broker.properties().await?,
            json!({"web_session_url": {
                "value": "https://serve.example/a", "inherit": false, "source": source,
            }})
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_worker_addresses_insert_exactly_once() -> Result<()> {
    require_nextest();
    let Some(broker) = broker().await? else {
        return Ok(());
    };
    let store = &broker.store;
    let session_id = &broker.session_id;
    let first_url = "https://first-worker.example/a";
    let second_url = "https://second-worker.example/a";
    let (first, second) = tokio::join!(
        store.record_web_session_url_if_absent(session_id, first_url),
        store.record_web_session_url_if_absent(session_id, second_url),
    );
    let (first, second) = (first?, second?);
    assert_ne!(first, second, "exactly one worker inserts the address");
    assert_eq!(
        broker.properties().await?,
        json!({"web_session_url": {
            "value": if first { first_url } else { second_url },
            "inherit": false,
            "source": "configured",
            "recorded_by": "worker",
        }})
    );
    Ok(())
}
