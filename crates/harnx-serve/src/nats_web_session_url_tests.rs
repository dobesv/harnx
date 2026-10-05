//! A session prompted through harnx-serve records where it opens in the Web UI.

use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use harnx_runtime::config::{Config, ConfigLock, NatsRouting};
use harnx_runtime::nats_session_metadata::{
    session_properties, PropertySource, SessionInitializer, SessionMetadata, SessionMetadataStore,
    WEB_SESSION_URL_PROPERTY,
};
use serde_json::json;

use crate::{test_support::TestConfigSandbox, Server};

const CLUSTER: &str = "web-url-test";

struct Fixture {
    _nats: harnx_test_bins::NatsServerHandle,
    _sandbox: TestConfigSandbox,
    config: Config,
    store: SessionMetadataStore,
}

async fn fixture(public_url: Option<&str>) -> Result<Option<Fixture>> {
    let Some(nats) = harnx_test_bins::spawn_nats_server().await? else {
        return Ok(None);
    };
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("plain", "You are plain.");
    sandbox.write_nats_server(CLUSTER, &format!("url: {}\n", nats.url()));
    let mut config = sandbox.config();
    config.nats_routing = NatsRouting::Cluster(CLUSTER.to_string());
    config.serve_public_url = public_url.map(str::to_string);
    let jetstream = config.nats_jetstream(CLUSTER).await?;
    let store = SessionMetadataStore::ensure(&jetstream, 1).await?;
    Ok(Some(Fixture {
        _nats: nats,
        _sandbox: sandbox,
        config,
        store,
    }))
}

impl Fixture {
    async fn new_session(&self) -> Result<String> {
        let session = harnx_runtime::nats_worker::new_remote_session_id();
        self.store
            .create(&SessionMetadata::new(
                &session,
                SessionInitializer::named("plain", Default::default()),
            ))
            .await?;
        Ok(session)
    }

    fn storage_key(session: &str) -> String {
        harnx_core::session_identity::session_key(Some("plain"), session)
    }

    /// Prompt `session` over JSON-RPC through a running server, sending
    /// `headers`, and return the Web UI address the session then records.
    async fn prompt(&self, session: &str, headers: &[(&str, &str)]) -> Result<Option<String>> {
        let config = Arc::new(ConfigLock::new(self.config.clone()));
        let server = Arc::new(Server::new(&config, PathBuf::from("web-assets")));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let handle = server.run(listener, crate::DEFAULT_DRAIN_TIMEOUT).await?;

        let mut request = reqwest::Client::new()
            .post(format!(
                "http://{address}/v1/agents/plain/sessions/{session}"
            ))
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "session/prompt",
                "params": {"text": "hello"},
            }));
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response: serde_json::Value = request.send().await?.json().await?;
        assert_eq!(response["result"]["status"], "accepted", "{response}");
        handle.shutdown().await?;

        let record = self
            .store
            .get(&Self::storage_key(session))
            .await?
            .expect("session metadata exists");
        Ok(session_properties(&record.metadata)?
            .text(WEB_SESSION_URL_PROPERTY)
            .map(str::to_string))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn prompt_records_the_address_the_proxy_forwarded() -> Result<()> {
    harnx_core::require_nextest();
    let Some(fixture) = fixture(None).await? else {
        return Ok(());
    };
    let session = fixture.new_session().await?;
    let recorded = fixture
        .prompt(
            &session,
            &[
                ("x-forwarded-host", "harnx.example.com"),
                ("x-forwarded-proto", "https"),
            ],
        )
        .await?;
    assert_eq!(
        recorded,
        Some(format!(
            "https://harnx.example.com/agents/plain/sessions/{session}"
        ))
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn configured_public_url_replaces_an_inferred_address_but_not_one_an_agent_set() -> Result<()>
{
    harnx_core::require_nextest();
    let Some(fixture) = fixture(Some("https://public.example.com/harnx/")).await? else {
        return Ok(());
    };
    let forwarded = [("x-forwarded-host", "proxy.example.com")];
    let configured =
        |session: &str| format!("https://public.example.com/harnx/agents/plain/sessions/{session}");

    let session = fixture.new_session().await?;
    assert_eq!(
        fixture.prompt(&session, &forwarded).await?,
        Some(configured(&session))
    );

    // Recorded before the public URL was configured.
    let session = fixture.new_session().await?;
    fixture
        .store
        .record_web_session_url(
            &Fixture::storage_key(&session),
            "http://harnx-serve.internal:8000/agents/plain/sessions/inferred",
            PropertySource::Inferred,
        )
        .await?;
    assert_eq!(
        fixture.prompt(&session, &forwarded).await?,
        Some(configured(&session))
    );

    let chosen = "https://elsewhere.example.com/agents/plain/sessions/chosen";
    let session = fixture.new_session().await?;
    let agent_choice = serde_json::from_value(json!({
        "set": [{"name": "web_session_url", "value": chosen}]
    }))?;
    fixture
        .store
        .update_session_properties(&Fixture::storage_key(&session), &agent_choice, None)
        .await?;
    assert_eq!(
        fixture.prompt(&session, &forwarded).await?.as_deref(),
        Some(chosen)
    );
    Ok(())
}
