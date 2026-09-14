mod common;

use anyhow::{Context, Result};
use harnx_core::session::SessionLogEntry;
use harnx_runtime::{
    config::Config,
    nats_admin::delete_remote_session,
    nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease},
    nats_session_log::NatsSessionLog,
    nats_session_metadata::SessionMetadataStore,
    NatsSession, NatsSessionConfig, SessionActivationRoute, SessionInitializer,
};

fn config(agent: &str) -> NatsSessionConfig {
    NatsSessionConfig {
        cluster: "local".into(),
        initializer: SessionInitializer::named(agent, Default::default()),
        session_id: Some("review-12345".into()),
        activation_route: SessionActivationRoute::ClusterShared,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_local_id_has_independent_history_leases_cancellation_and_deletion() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = common::spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let js = async_nats::jetstream::new(client.clone());
    let create = |agent| {
        NatsSession::new(
            config(agent),
            client.clone(),
            js.clone(),
            harnx_runtime::utils::create_abort_signal(),
        )
    };
    let (alpha, beta) = tokio::try_join!(create("alpha"), create("beta"))?;
    assert_eq!(alpha.session_id(), "review-12345");
    assert_eq!(beta.session_id(), "review-12345");
    assert_ne!(alpha.storage_key(), beta.storage_key());

    let alpha_lease = acquire_lease(&js, &alpha).await?;
    let beta_lease = acquire_lease(&js, &beta).await?;
    let alpha_prompt = alpha.enqueue_text("alpha review").await?;
    let beta_prompt = beta.enqueue_text("beta review").await?;
    assert_eq!(alpha_prompt.user_msg_seq(), 1);
    assert_eq!(beta_prompt.user_msg_seq(), 1);

    let metadata = SessionMetadataStore::ensure(&js, 1).await?;
    assert_metadata_isolation(&metadata, &alpha).await?;

    let resumed = create("alpha").await?;
    assert_eq!(resumed.storage_key(), alpha.storage_key());
    let alpha_log = NatsSessionLog::new(js.clone(), alpha.storage_key());
    let beta_log = NatsSessionLog::new(js.clone(), beta.storage_key());
    assert_independent_history(&alpha_log, &beta_log).await?;

    let attachment_cid = upload_shared_attachment(&js, [&alpha, &beta]).await?;
    alpha.request_cancel(Default::default()).await?;
    assert!(alpha
        .execution_store()
        .current(alpha.storage_key())
        .await?
        .unwrap()
        .state
        .cancelling());
    assert!(beta
        .execution_store()
        .current(beta.storage_key())
        .await?
        .unwrap()
        .state
        .accepts_work());
    alpha_lease.release().await?;
    let admin = admin_config(server.url());
    let deleted = delete_remote_session(&admin, "local", "alpha", "review-12345").await?;
    assert!(deleted.stream_deleted);
    assert_eq!(deleted.attachments_deleted, 1);
    assert_beta_survived_deletion(&js, &beta, &beta_lease, &attachment_cid).await?;
    beta_lease.release().await?;
    Ok(())
}

async fn upload_shared_attachment(
    js: &async_nats::jetstream::Context,
    sessions: [&NatsSession; 2],
) -> Result<String> {
    use harnx_core::message::{ImageUrl, MessageContent, MessageContentPart};
    let data_url = format!(
        "data:text/plain;base64,{}",
        harnx_core::crypto::base64_encode(b"review attachment")
    );
    let cid = harnx_core::attachments::cid_for_data_url(&data_url);
    for session in sessions {
        let mut content = MessageContent::Array(vec![MessageContentPart::ImageUrl {
            image_url: ImageUrl {
                url: data_url.clone(),
            },
        }]);
        harnx_runtime::nats_attachments::externalize_message_attachments(
            harnx_runtime::nats_attachments::AttachmentLocation::new(js, 1, session.storage_key()),
            &mut content,
            None,
        )
        .await?;
    }
    Ok(cid)
}

#[tokio::test]
async fn reserved_inline_name_cannot_create_a_named_session() -> Result<()> {
    let Some(server) = common::spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let js = async_nats::jetstream::new(client.clone());
    let result = NatsSession::new(
        config(harnx_core::agent_config::TEMP_AGENT_NAME),
        client,
        js,
        harnx_runtime::utils::create_abort_signal(),
    )
    .await;
    assert!(
        result.is_err(),
        "reserved inline name must not create a second storage identity"
    );
    Ok(())
}

async fn assert_metadata_isolation(
    metadata: &SessionMetadataStore,
    alpha: &NatsSession,
) -> Result<()> {
    metadata
        .patch(alpha.storage_key(), |record| {
            record.title.value = Some("alpha title".into());
            Ok(())
        })
        .await?;
    let beta_record = metadata
        .get_for_agent("review-12345", "beta")
        .await?
        .unwrap();
    assert_eq!(beta_record.metadata.title.value, None);
    assert_eq!(metadata.list().await?.len(), 2);

    Ok(())
}

fn admin_config(url: &str) -> Config {
    let mut admin = Config::default();
    admin
        .nats_servers
        .push(harnx_runtime::config::NatsServerConfig {
            name: "local".into(),
            url: url.into(),
            token: None,
            replicas: Some(1),
            tls: Some(false),
            tls_cert: None,
            tls_key: None,
            tls_ca: None,
            agents: Vec::new(),
        });
    admin
}

async fn assert_independent_history(
    alpha_log: &NatsSessionLog,
    beta_log: &NatsSessionLog,
) -> Result<()> {
    for (log, text) in [(alpha_log, "alpha review"), (beta_log, "beta review")] {
        let entries = log.load_events_async().await?;
        assert_eq!(entries.len(), 1);
        assert!(
            matches!(&entries[0].1, SessionLogEntry::Message { content, .. } if content.to_text() == text)
        );
    }
    Ok(())
}

async fn acquire_lease(
    js: &async_nats::jetstream::Context,
    session: &NatsSession,
) -> Result<NatsSessionLease> {
    NatsSessionLease::acquire(NatsLeaseAcquireParams {
        jetstream: js.clone(),
        session_id: session.storage_key(),
        worker_id: "same-worker".into(),
        generation: 1,
        config: NatsLeaseConfig::default(),
        session_metadata: None,
    })
    .await?
    .context("each agent can independently own its session lease")
}

async fn assert_beta_survived_deletion(
    js: &async_nats::jetstream::Context,
    beta: &NatsSession,
    beta_lease: &NatsSessionLease,
    attachment_cid: &String,
) -> Result<()> {
    let metadata = SessionMetadataStore::ensure(js, 1).await?;
    let beta_log = NatsSessionLog::new(js.clone(), beta.storage_key());
    let hydrated = tempfile::tempdir()?;
    harnx_runtime::nats_attachments::hydrate_attachment_refs(
        harnx_runtime::nats_attachments::AttachmentLocation::new(js, 1, beta.storage_key()),
        hydrated.path(),
        std::slice::from_ref(attachment_cid),
    )
    .await?;
    assert_eq!(
        harnx_core::attachments::read_attachment_async(hydrated.path(), attachment_cid)
            .await?
            .0,
        b"review attachment"
    );
    assert!(metadata
        .get_for_agent("review-12345", "alpha")
        .await?
        .is_none());
    assert!(metadata
        .get_for_agent("review-12345", "beta")
        .await?
        .is_some());
    assert_eq!(beta_log.load_events_async().await?.len(), 1);
    assert!(beta_lease.is_held());
    assert!(beta
        .execution_store()
        .current(beta.storage_key())
        .await?
        .is_some());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_agent_controls_completion_and_info_even_with_other_active_agent() -> Result<()> {
    let Some(server) = common::spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let js = async_nats::jetstream::new(client.clone());
    for (agent, id) in [("alpha", "alpha-only"), ("beta", "beta-only")] {
        let session = NatsSession::new(
            NatsSessionConfig {
                session_id: Some(id.into()),
                ..config(agent)
            },
            client.clone(),
            js.clone(),
            harnx_runtime::utils::create_abort_signal(),
        )
        .await?;
        session.enqueue_text(agent).await?;
    }
    let mut cfg = admin_config(server.url());
    cfg.set_remote_agent("unrelated".into(), "unreachable-cluster".into());
    assert_eq!(
        cfg.list_sessions_for_completion("alpha@local").await,
        vec!["alpha-only"]
    );
    assert_eq!(
        cfg.list_sessions_for_completion("beta@local").await,
        vec!["beta-only"]
    );
    let (broker, metadata) =
        harnx_runtime::config::session_metadata_for_agent(&cfg, "alpha@local", "alpha-only")
            .await?;
    let entries = NatsSessionLog::new(broker, metadata.storage_key())
        .load_events_async()
        .await?;
    assert!(serde_json::to_string(&entries)?.contains("alpha"));
    assert!(
        harnx_runtime::config::session_metadata_for_agent(&cfg, "beta@local", "alpha-only")
            .await
            .is_err()
    );
    let global = std::sync::Arc::new(parking_lot::RwLock::new(cfg));
    for format in ["json", "yaml"] {
        let mut output = Vec::new();
        harnx_runtime::commands::run_command_with_output(
            &global,
            harnx_runtime::utils::create_abort_signal(),
            &format!(".info session alpha@local alpha-only --format {format}"),
            &mut output,
        )
        .await?;
        let rendered: serde_json::Value = serde_yaml::from_slice(&output)?;
        assert_eq!(rendered["session_id"], "alpha-only");
        assert_eq!(rendered["agent"]["name"], "alpha");
    }
    Ok(())
}
