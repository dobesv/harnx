use super::*;
use crate::support::Broker;
use harnx_a2a_server::store::A2aStore;
use harnx_runtime::{
    nats_lease::NatsLeaseAcquireParams, nats_session_metadata::SessionMetadataStore,
};

#[tokio::test]
async fn authoritative_reads_work_when_follower_direct_get_subject_is_forbidden() -> Result<()> {
    harnx_core::require_nextest();
    tokio::time::timeout(crate::support::DEADLINE, leader_only())
        .await
        .context("leader permissions deadline")?
}
async fn leader_only() -> Result<()> {
    let config = r#"
no_auth_user: admin
 authorization { users: [
 {user: admin},
 {user: replica, password: test, permissions: {
   publish: {allow: ["$JS.API.INFO", "$JS.API.STREAM.>", "$JS.API.CONSUMER.>", "$JS.ACK.>", "$KV.harnx_sessions.>", "$KV.harnx_leases.>"]},
   subscribe: {allow: ["_INBOX.>"]}
 }}] }
"#;
    let (_broker, url, _) = Broker::start_with_config(config).await?;
    let (errors, mut received) = tokio::sync::mpsc::unbounded_channel();
    let client = async_nats::ConnectOptions::new()
        .user_and_password("replica".into(), "test".into())
        .event_callback(move |event| {
            let errors = errors.clone();
            async move {
                if let async_nats::Event::ServerError(error) = event {
                    let _ = errors.send(error.to_string());
                }
            }
        })
        .connect(url)
        .await?;
    let js = async_nats::jetstream::new(client.clone());
    let metadata = SessionMetadataStore::ensure(&js, 1).await?;
    let store = A2aStore::new(metadata.clone());
    let lease = assert_leader_context(js, &store).await?;
    assert!(
        received.try_recv().is_err(),
        "authority API required forbidden subjects"
    );
    // Prove the account doesn't permit async-nats follower/direct KV gets.
    client
        .publish("$JS.API.DIRECT.GET.KV_harnx_sessions", "{}".into())
        .await?;
    client.flush().await?;
    let violation = received.recv().await.context("permission event")?;
    assert!(
        violation.contains("Permissions Violation") && violation.contains("DIRECT.GET"),
        "{violation}"
    );
    lease.release().await?;
    Ok(())
}

async fn assert_leader_context(
    js: async_nats::jetstream::Context,
    store: &A2aStore,
) -> Result<NatsSessionLease> {
    let lease = NatsSessionLease::acquire_scoped(
        NatsLeaseAcquireParams {
            jetstream: js,
            session_id: STORAGE,
            worker_id: "restricted-owner".into(),
            generation: 1,
            config: Default::default(),
            session_metadata: None,
        },
        "a2a",
    )
    .await?
    .context("lease")?;
    let write = store
        .prepare_context_claim(
            ContextIdentity {
                storage_key: STORAGE,
                local_id: LOCAL,
            },
            &lease,
            "claim",
        )
        .await?;
    let committed = store.commit_context(&write).await?;
    assert_eq!(
        store
            .read_context(STORAGE)
            .await?
            .context("leader read")?
            .revision,
        committed.revision
    );
    Ok(lease)
}
