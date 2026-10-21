use crate::{access_agents::Fixture as HttpFixture, common};
use anyhow::Result;
use harnx_core::session::SessionLogEntry;
use harnx_runtime::{
    nats_session_log::NatsSessionLog,
    nats_session_metadata::{SessionInitializer, SessionMetadata, SessionMetadataStore},
};
use reqwest::{Method, RequestBuilder, StatusCode};
use serde_json::{json, Value};

const RULES: &str = "rules:\n  - agents: [allowed]\n    scopes: [prompt]\n    users: [alice, bob]\n  - agents: [allowed]\n    scopes: [admin]\n    users: [admin]\n";

pub(super) struct Fixture {
    pub http: HttpFixture,
    pub log: NatsSessionLog,
    pub store: SessionMetadataStore,
    _nats: common::NatsServerHandle,
}

pub(super) fn entries() -> Vec<(u64, SessionLogEntry)> {
    serde_json::from_str(include_str!(
        "../../../../harnx-core/tests/fixtures/compaction-history.json"
    ))
    .unwrap()
}

pub(super) async fn fixture(rules_on: bool) -> Result<Option<Fixture>> {
    harnx_core::require_nextest();
    let Some(nats) = common::spawn_nats_server().await? else {
        return Ok(None);
    };
    let client = async_nats::connect(nats.url()).await?;
    let jetstream = async_nats::jetstream::new(client);
    let store = SessionMetadataStore::ensure(&jetstream, 1).await?;
    let http = HttpFixture::start_with_nats(rules_on.then_some(RULES), nats.url()).await?;
    let initializer =
        SessionInitializer::named("allowed", Default::default()).with_user_id("alice");
    store
        .create(&SessionMetadata::new("history", initializer.clone()))
        .await?;
    store
        .create(&SessionMetadata::new("other", initializer))
        .await?;
    let log = NatsSessionLog::new(jetstream, storage_key("history")).with_replicas(1);
    for (seq, entry) in entries() {
        assert_eq!(log.append_event_async(&entry).await?, seq);
    }
    Ok(Some(Fixture {
        http,
        log,
        store,
        _nats: nats,
    }))
}

impl Fixture {
    pub fn request(&self, method: Method, suffix: &str, user: &str) -> RequestBuilder {
        self.http
            .client
            .request(
                method,
                format!("{}/v1/agents/allowed/sessions/{suffix}", self.http.base),
            )
            .header("x-user", user)
            .header("accept", "application/json")
    }

    pub async fn rpc(&self, method: &str, params: Value) -> Result<reqwest::Response> {
        Ok(self
            .request(Method::POST, "history", "alice")
            .json(&json!({"jsonrpc":"2.0","id":7,"method":method,"params":params}))
            .send()
            .await?)
    }

    pub async fn history(&self, params: Value) -> Result<Value> {
        let response = self.rpc("session/history", params).await?;
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = response.json().await?;
        assert_eq!(body["id"], 7);
        Ok(body["result"].clone())
    }

    pub async fn walk(&self, compaction: u64, limit: usize) -> Result<Vec<Value>> {
        let mut page = self
            .history(json!({"before_compaction":compaction,"limit":limit}))
            .await?;
        let mut rows = Vec::new();
        loop {
            assert_rounds(&page);
            rows.extend(page["history_snapshot"].as_array().unwrap().iter().cloned());
            if page["next_cursor"].is_null() {
                break;
            }
            page = self.history(json!({"cursor":page["next_cursor"]})).await?;
        }
        Ok(rows)
    }
}

pub(super) fn assert_rounds(page: &Value) {
    let rows = page["history_snapshot"].as_array().unwrap();
    let calls: Vec<_> = rows
        .iter()
        .filter_map(|row| row["toolCalls"].as_array())
        .flatten()
        .map(|call| &call["id"])
        .collect();
    let results: Vec<_> = rows
        .iter()
        .filter(|row| row["role"] == "tool")
        .map(|row| &row["toolCallId"])
        .collect();
    assert_eq!(calls, results);
}

pub(super) fn contents(rows: &[Value]) -> Vec<&str> {
    rows.iter()
        .filter_map(|row| row["content"].as_str())
        .collect()
}

pub(super) fn storage_key(id: &str) -> String {
    harnx_core::session_identity::session_key(Some("allowed"), id)
}
