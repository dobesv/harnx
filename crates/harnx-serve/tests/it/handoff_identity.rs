//! Caller identity reads on the serve handoff path, against isolated NATS.

use crate::common;
use anyhow::{Context, Result};
use harnx_core::user_principals::{from_resolved, UserPrincipals};
use harnx_runtime::{
    config::Config,
    nats_session_metadata::{SessionInitializer, SessionMetadata, SessionMetadataStore},
};
use harnx_serve::test_support::{handoff_identity_for_test, TestConfigSandbox};
use serde_json::json;

const CLUSTER: &str = "handoff-identity-test";

struct Fixture {
    _sandbox: TestConfigSandbox,
    config: Config,
    store: SessionMetadataStore,
    _nats: common::NatsServerHandle,
}

async fn start_fixture() -> Result<Option<Fixture>> {
    harnx_core::require_nextest();
    let Some(nats) = common::spawn_nats_server().await? else {
        return Ok(None);
    };
    let sandbox = TestConfigSandbox::new();
    sandbox.write_nats_server(CLUSTER, &format!("url: {:?}\n", nats.url()));
    let config = sandbox.config();
    let client = async_nats::connect(nats.url()).await?;
    let store = SessionMetadataStore::ensure(&async_nats::jetstream::new(client), 1).await?;
    Ok(Some(Fixture {
        _sandbox: sandbox,
        config,
        store,
        _nats: nats,
    }))
}

async fn insert_source(
    store: &SessionMetadataStore,
    session_id: &str,
    properties: serde_json::Value,
) -> Result<()> {
    let mut metadata = SessionMetadata::new(
        session_id,
        SessionInitializer::named("source-agent", Default::default()),
    );
    metadata.extensions.insert(
        harnx_runtime::nats_session_metadata::SESSION_PROPERTIES_NAMESPACE.into(),
        properties,
    );
    store.create(&metadata).await?;
    Ok(())
}

async fn lookup_identity(
    config: &Config,
    session_id: &str,
) -> Result<(Option<String>, Option<UserPrincipals>)> {
    handoff_identity_for_test(config, CLUSTER, "source-agent", session_id).await
}

#[tokio::test]
async fn handoff_lookup_reads_source_identity_and_session_store_round_trips_it() -> Result<()> {
    let Some(fixture) = start_fixture().await? else {
        return Ok(());
    };
    let principals = from_resolved(
        Some("alice"),
        Some(&["alice".into(), "alice@example.test".into()]),
        &["engineering".into(), "on-call".into()],
        &["reviewer".into()],
    );
    let initializer = SessionInitializer::named("source-agent", Default::default())
        .with_user_id("alice")
        .with_user_principals(&principals);
    let metadata = SessionMetadata::new("source-with-identity", initializer);
    fixture.store.create(&metadata).await?;

    let (user_id, snapshot) = lookup_identity(&fixture.config, "source-with-identity").await?;
    assert_eq!(user_id.as_deref(), Some("alice"));
    assert_eq!(snapshot, Some(principals.clone()));

    let target_initializer = SessionInitializer::named("target-agent", Default::default())
        .with_user_id(user_id.expect("source owner"))
        .with_user_principals(&snapshot.expect("source principal snapshot"));
    fixture
        .store
        .create(&SessionMetadata::new("target-session", target_initializer))
        .await?;
    let target = fixture
        .store
        .get_for_agent("target-session", "target-agent")
        .await?
        .context("handoff target metadata")?;
    let properties = harnx_runtime::nats_session_metadata::session_properties(&target.metadata)?;
    assert_eq!(properties.text("user_id"), Some("alice"));
    assert_eq!(properties.user_principals(), principals);
    Ok(())
}

#[tokio::test]
async fn handoff_lookup_treats_missing_source_record_as_anonymous() -> Result<()> {
    let Some(fixture) = start_fixture().await? else {
        return Ok(());
    };
    let (user_id, snapshot) = lookup_identity(&fixture.config, "missing-source").await?;
    assert!(user_id.is_none());
    assert!(snapshot.is_none());
    Ok(())
}

#[tokio::test]
async fn handoff_lookup_rejects_malformed_existing_source_properties() -> Result<()> {
    let Some(fixture) = start_fixture().await? else {
        return Ok(());
    };
    insert_source(
        &fixture.store,
        "malformed-source",
        json!("not-an-extension-object"),
    )
    .await?;
    let error = lookup_identity(&fixture.config, "malformed-source")
        .await
        .expect_err("malformed existing properties must fail handoff");
    assert!(
        format!("{error:#}").contains("decode source session properties for handoff"),
        "unexpected error: {error:#}"
    );
    Ok(())
}

#[tokio::test]
async fn handoff_lookup_propagates_metadata_store_connection_errors() -> Result<()> {
    harnx_core::require_nextest();
    let sandbox = TestConfigSandbox::new();
    sandbox.write_nats_server(CLUSTER, "url: nats://127.0.0.1:1\n");
    let config = sandbox.config();
    let error = handoff_identity_for_test(&config, CLUSTER, "source-agent", "source")
        .await
        .expect_err("unavailable broker must fail identity lookup");
    assert!(format!("{error:#}").contains("NATS cluster 'handoff-identity-test' is unavailable"));
    Ok(())
}
