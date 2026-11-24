use super::{require_nextest, storage_key};
use crate::common::{spawn_configured_nats_server, NatsServerHandle};
use anyhow::{Context, Result};
use async_nats::jetstream::{self, context::GetStreamErrorKind, stream, ErrorCode};
use futures_util::StreamExt;
use harnx_core::user_principals::{self, UserPrincipals};
use harnx_runtime::{
    nats_session::{create_or_read_session_metadata, SessionMetadataClaim},
    nats_session_log::{stream_name_for_session, subject_for_session},
    nats_session_metadata::{
        activity_key, metadata_key, session_properties, user_principals, MetadataRecord,
        SessionInitializer, SessionMetadata, SessionMetadataStore, SESSION_METADATA_BUCKET,
    },
};
use std::time::Duration;

const SESSION_ID: &str = "client-chosen";
const REDIRECT: &str = "tests.session_claim_request";
const TIMEOUT: Duration = Duration::from_secs(20);

struct ClaimFixture {
    _server: NatsServerHandle,
    store: SessionMetadataStore,
    js: jetstream::Context,
    key: String,
    stream_name: String,
}

impl ClaimFixture {
    // qual:test_helper
    async fn start(redirect: Option<&str>) -> Result<Option<Self>> {
        require_nextest();
        let mut config = "jetstream: { domain: claim }\n".to_string();
        if let Some(api) = redirect {
            config.push_str(&format!(
                "mappings: {{ \"$JS.API.{api}\": \"{REDIRECT}\" }}"
            ));
        }
        let Some(server) = spawn_configured_nats_server(&config).await? else {
            return Ok(None);
        };
        let client = async_nats::connect(server.url()).await?;
        let mut js = jetstream::new(client.clone());
        js.set_timeout(TIMEOUT);
        let store = SessionMetadataStore::ensure(&js, 1).await?;
        let key = storage_key(SESSION_ID);
        eprintln!("running against NATS {}", client.server_info().version);
        Ok(Some(Self {
            _server: server,
            store,
            js: jetstream::with_domain(client, "claim"),
            stream_name: stream_name_for_session(&key),
            key,
        }))
    }

    // qual:test_helper
    async fn claim(&self, owner: &str) -> Result<SessionMetadataClaim> {
        create_or_read_session_metadata(&self.store, SESSION_ID, &initializer(owner)).await
    }

    // qual:test_helper
    async fn seed_winner(&self) -> Result<u64> {
        let metadata = SessionMetadata::new(SESSION_ID, initializer("alice"));
        self.store.create(&metadata).await?.context("seed winner")
    }

    // qual:test_helper
    async fn seed_empty_stream(&self) -> Result<()> {
        self.store
            .jetstream()
            .create_stream(stream::Config {
                name: self.stream_name.clone(),
                subjects: vec![subject_for_session(&self.key)],
                ..Default::default()
            })
            .await?;
        Ok(())
    }

    // qual:test_helper
    async fn assert_stream_missing(&self) -> Result<()> {
        let error = self.js.get_stream(&self.stream_name).await.unwrap_err();
        assert!(matches!(error.kind(), GetStreamErrorKind::JetStream(e)
            if e.kind() == ErrorCode::STREAM_NOT_FOUND));
        Ok(())
    }
}

/// Pause a JetStream request via a broker subject mapping, without adding a
/// race hook to the public API.
struct Redirect<'a> {
    client: async_nats::Client,
    api: &'a str,
    requests: async_nats::Subscriber,
}

impl<'a> Redirect<'a> {
    // qual:test_helper
    async fn start(client: async_nats::Client, api: &'a str) -> Result<Self> {
        let requests = client.subscribe(REDIRECT).await?;
        client.flush().await?;
        Ok(Self {
            client,
            api,
            requests,
        })
    }

    // qual:test_helper
    async fn receive(&mut self) -> Result<async_nats::Message> {
        tokio::time::timeout(TIMEOUT, self.requests.next())
            .await?
            .context("missing redirected JetStream request")
    }

    /// Forward around the mapping. The test decides when the original
    /// requester gets the broker's response.
    // qual:test_helper
    async fn broker_reply(&self, request: &async_nats::Message) -> Result<bytes::Bytes> {
        let reply = self
            .client
            .request(
                format!("$JS.claim.API.{}", self.api),
                request.payload.clone(),
            )
            .await?;
        Ok(reply.payload)
    }

    // qual:test_helper
    async fn respond(&self, request: async_nats::Message, payload: bytes::Bytes) -> Result<()> {
        self.client
            .publish(request.reply.context("missing reply subject")?, payload)
            .await?;
        Ok(())
    }
}

// qual:test_helper
async fn start_with_redirect(api: &str) -> Result<Option<(ClaimFixture, Redirect<'_>)>> {
    let Some(fixture) = ClaimFixture::start(Some(api)).await? else {
        return Ok(None);
    };
    let requests = Redirect::start(fixture.js.client(), api).await?;
    Ok(Some((fixture, requests)))
}

fn stream_info_api() -> String {
    format!(
        "STREAM.INFO.{}",
        stream_name_for_session(&storage_key(SESSION_ID))
    )
}

fn principals(owner: &str) -> UserPrincipals {
    user_principals::from_resolved(Some(owner), None, &[format!("{owner}-team")], &[])
}

fn initializer(owner: &str) -> SessionInitializer {
    SessionInitializer::named("test-agent", Default::default())
        .with_user_id(owner)
        .with_user_principals(&principals(owner))
}

// qual:test_helper
fn assert_owner(record: &MetadataRecord, owner: &str) -> Result<()> {
    let properties = session_properties(&record.metadata)?;
    assert_eq!(properties.text("user_id"), Some(owner));
    assert_eq!(user_principals(&record.metadata), principals(owner));
    Ok(())
}

#[tokio::test]
async fn claims_absent_id_with_request_identity() -> Result<()> {
    let Some(fixture) = ClaimFixture::start(None).await? else {
        return Ok(());
    };
    let SessionMetadataClaim::Created(record) = fixture.claim("alice").await? else {
        panic!("a fresh ID must be created");
    };
    assert_owner(&record, "alice")?;
    assert_eq!(record.metadata.session_id, SESSION_ID);
    let persisted = fixture.store.get(&fixture.key).await?.unwrap();
    assert_eq!(persisted.metadata, record.metadata);
    assert_eq!(persisted.revision, record.revision);
    assert!(fixture.store.get_activity(&fixture.key).await?.is_some());
    fixture.assert_stream_missing().await
}

#[tokio::test]
async fn returns_existing_without_repairing_activity() -> Result<()> {
    let Some(fixture) = ClaimFixture::start(None).await? else {
        return Ok(());
    };
    let revision = fixture.seed_winner().await?;
    fixture
        .store
        .kv_store()
        .purge(activity_key(&fixture.key))
        .await?;

    let SessionMetadataClaim::Existing(record) = fixture.claim("bob").await? else {
        panic!("the existing owner must win even without a transcript");
    };
    assert_owner(&record, "alice")?;
    assert_eq!(record.revision, revision);
    assert_eq!(
        fixture.store.get(&fixture.key).await?.unwrap().revision,
        revision
    );
    assert!(fixture.store.get_activity(&fixture.key).await?.is_none());
    fixture.assert_stream_missing().await
}

#[tokio::test]
async fn rejects_empty_and_nonempty_orphan_streams() -> Result<()> {
    let Some(fixture) = ClaimFixture::start(None).await? else {
        return Ok(());
    };
    fixture.seed_empty_stream().await?;
    for message_count in [0, 1] {
        if message_count == 1 {
            // Not a transcript entry: the orphan check must not read or
            // deserialize any messages to decide whether the stream exists.
            fixture
                .js
                .publish(subject_for_session(&fixture.key), "not a log entry".into())
                .await?
                .await?;
        }
        let before = fixture.js.get_stream(&fixture.stream_name).await?;
        let outcome = fixture.claim("bob").await?;
        assert!(
            matches!(outcome, SessionMetadataClaim::Orphaned),
            "{outcome:?}"
        );
        assert!(fixture.store.get(&fixture.key).await?.is_none());
        assert!(fixture.store.get_activity(&fixture.key).await?.is_none());
        let after = fixture.js.get_stream(&fixture.stream_name).await?;
        assert_eq!(after.cached_info().state.messages, message_count);
        assert_eq!(
            after.cached_info().state.last_sequence,
            before.cached_info().state.last_sequence
        );
        assert_eq!(after.cached_info().created, before.cached_info().created);
    }
    Ok(())
}

#[tokio::test]
async fn rereads_metadata_after_observing_a_transcript() -> Result<()> {
    let api = stream_info_api();
    let Some((fixture, mut requests)) = start_with_redirect(&api).await? else {
        return Ok(());
    };
    let race = async {
        let request = requests.receive().await?;
        let revision = fixture.seed_winner().await?;
        fixture.seed_empty_stream().await?;
        let reply = requests.broker_reply(&request).await?;
        requests.respond(request, reply).await?;
        Ok(revision)
    };
    let (outcome, revision) = tokio::try_join!(fixture.claim("bob"), race)?;
    let SessionMetadataClaim::Existing(record) = outcome else {
        panic!("the winner's stream was mistaken for an orphan: {outcome:?}");
    };
    assert_owner(&record, "alice")?;
    assert_eq!(record.revision, revision);
    Ok(())
}

#[tokio::test]
async fn concurrent_claims_agree_on_the_owner() -> Result<()> {
    let api = stream_info_api();
    let Some((fixture, mut requests)) = start_with_redirect(&api).await? else {
        return Ok(());
    };
    let race = async {
        // All three initial leader reads must see absence before any caller
        // attempts a create, forcing the losing callers through the collision.
        let pending = [
            requests.receive().await?,
            requests.receive().await?,
            requests.receive().await?,
        ];
        for request in pending {
            let reply = requests.broker_reply(&request).await?;
            requests.respond(request, reply).await?;
        }
        Ok(())
    };
    let (alice, bob, carol, ()) = tokio::try_join!(
        fixture.claim("alice"),
        fixture.claim("bob"),
        fixture.claim("carol"),
        race
    )?;
    let winner = fixture.store.get(&fixture.key).await?.unwrap();
    let outcomes = [(alice, "alice"), (bob, "bob"), (carol, "carol")];
    let mut created = 0;
    for (outcome, contender) in outcomes {
        let record = match outcome {
            SessionMetadataClaim::Created(record) => {
                created += 1;
                assert_owner(&record, contender)?;
                record
            }
            SessionMetadataClaim::Existing(record) => record,
            other => panic!("unexpected claim outcome: {other:?}"),
        };
        assert_eq!(record.revision, winner.revision);
        assert_eq!(record.metadata, winner.metadata);
    }
    assert_eq!(created, 1);
    fixture.assert_stream_missing().await
}

#[tokio::test]
async fn reports_vanished_collision_winner() -> Result<()> {
    let api = format!("STREAM.MSG.GET.KV_{SESSION_METADATA_BUCKET}");
    let Some((fixture, mut requests)) = start_with_redirect(&api).await? else {
        return Ok(());
    };
    let race = async {
        let initial_read = requests.receive().await?;
        let absent = requests.broker_reply(&initial_read).await?;
        fixture.seed_winner().await?;
        requests.respond(initial_read, absent).await?;

        let read_back = requests.receive().await?;
        fixture
            .store
            .kv_store()
            .purge(metadata_key(&fixture.key))
            .await?;
        let purged = requests.broker_reply(&read_back).await?;
        requests.respond(read_back, purged).await
    };
    let (outcome, ()) = tokio::try_join!(fixture.claim("bob"), race)?;
    assert!(
        matches!(outcome, SessionMetadataClaim::Vanished),
        "{outcome:?}"
    );
    fixture.assert_stream_missing().await
}

#[tokio::test]
async fn propagates_transcript_probe_broker_error() -> Result<()> {
    let api = stream_info_api();
    let Some((fixture, mut requests)) = start_with_redirect(&api).await? else {
        return Ok(());
    };
    let fail_probe = async {
        let request = requests.receive().await?;
        // NATS accepted the request, but JetStream cannot answer it. This must
        // not be interpreted like the authoritative stream-not-found response.
        let unavailable = r#"{"error":{"code":503,"err_code":10008,"description":"JetStream cluster not available"}}"#;
        requests.respond(request, unavailable.into()).await
    };
    let (outcome, sent) = tokio::join!(fixture.claim("bob"), fail_probe);
    sent?;
    let error = outcome.expect_err("broker failure must be an error");
    let broker = error
        .downcast_ref::<jetstream::context::GetStreamError>()
        .unwrap();
    assert!(matches!(broker.kind(), GetStreamErrorKind::JetStream(e)
        if e.kind() == ErrorCode::CLUSTER_NOT_AVAILABLE));
    assert!(fixture.store.get(&fixture.key).await?.is_none());
    assert!(fixture.store.get_activity(&fixture.key).await?.is_none());
    fixture.assert_stream_missing().await
}
