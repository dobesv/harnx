use super::support::{WorkerDaemonHandle, WorkerPair};
use super::*;
use anyhow::{ensure, Context};
use futures_util::StreamExt;
use harnx_core::instance::ServerScope;
use harnx_runtime::config::ToolServerConfig;
use harnx_runtime::nats_worker::tool_reservation::*;
use harnx_toolset_server::{registration_key, TOOL_REGISTRY_BUCKET};
use tokio_util::task::AbortOnDropHandle;

struct Fixture {
    worker: WorkerDaemonHandle,
    client: async_nats::Client,
    js: async_nats::jetstream::Context,
    calls: Arc<AtomicUsize>,
    subject: String,
    _env: Vec<EnvGuard>,
    _server: common::NatsServerHandle,
}

fn fs_tools_binary() -> Result<std::path::PathBuf> {
    let binary = std::env::current_exe()?
        .parent()
        .context("test binary dir")?
        .parent()
        .context("target debug dir")?
        .join(format!("harnx-fs-tools{}", std::env::consts::EXE_SUFFIX));
    ensure!(
        binary.is_file(),
        "build workspace first: {} is missing",
        binary.display()
    );
    Ok(binary)
}

fn build_fixture_env(server_url: &str, unmanaged: bool) -> Vec<EnvGuard> {
    let mut env = vec![
        EnvGuard::set("HARNX_NATS_URL", server_url),
        EnvGuard::set("HARNX_NATS_TOKEN", ""),
    ];
    if unmanaged {
        env.push(EnvGuard::set(
            "HARNX_SERVER_SCOPE",
            "unmanaged-reservation-test",
        ));
    }
    env
}

fn build_fixture_config(
    server_url: &str,
    targeted: bool,
    binary: &std::path::Path,
) -> harnx_runtime::config::GlobalConfig {
    let config = local_nats_runtime_config(server_url);
    {
        let mut config = config.write();
        config.clients.clear();
        if targeted {
            config.model = Default::default();
        }
        config.use_tools = Some(vec!["fs_*".to_owned()]);
        config.tool_servers = vec![ToolServerConfig {
            name: "fs".to_owned(),
            command: binary.to_string_lossy().into_owned(),
            args: vec!["--name".to_owned(), "fs".to_owned()],
            env: Default::default(),
            enabled: true,
            description: None,
            package: None,
            hooks: None,
        }];
    }
    config
}

fn build_daemon_and_subject(
    ttl: Duration,
    targeted: bool,
    unmanaged: bool,
) -> Result<(WorkerDaemonConfig, String)> {
    let daemon = if targeted {
        WorkerDaemonConfig::local("reservation-worker")?
    } else if unmanaged {
        WorkerDaemonConfig::new("local", "reservation-worker")
    } else {
        WorkerDaemonConfig::managing("local", "reservation-worker")
    }
    .with_tool_reservation_timing_for_test(ttl, Duration::ZERO);
    let subject = if targeted {
        targeted_reserve_subject(harnx_runtime::nats_worker::LocalWorkerTarget::new(
            "__local__",
            "reservation-worker",
        )?)
    } else {
        reserve_subject("local")
    };
    Ok((daemon, subject))
}

impl Fixture {
    async fn start(ttl: Duration, targeted: bool, unmanaged: bool) -> Result<Option<Self>> {
        let Some(server) = require_nats_server().await? else {
            return Ok(None);
        };
        let binary = fs_tools_binary()?;
        let env = build_fixture_env(server.url(), unmanaged);
        let config = build_fixture_config(server.url(), targeted, &binary);
        let (daemon, subject) = build_daemon_and_subject(ttl, targeted, unmanaged)?;

        let client = async_nats::ConnectOptions::new()
            .request_timeout(Some(CI_SAFE_TIMEOUT))
            .connect(server.url())
            .await?;
        let js = async_nats::jetstream::new(client.clone());
        let calls = Arc::new(AtomicUsize::new(0));
        let readiness = harnx_healthz::Readiness::default();
        let mut worker = AbortOnDropHandle::new(tokio::spawn({
            let readiness = readiness.clone();
            let call_fn = counting_stub_call_fn(Arc::clone(&calls));
            async move { run_worker_daemon(config, daemon, Some(call_fn), Some(readiness)).await }
        }));
        tokio::select! {
            result = wait_until(CI_SAFE_TIMEOUT, || readiness.is_ready()) => result?,
            stopped = &mut worker => anyhow::bail!("reservation worker failed: {stopped:?}"),
        }
        seed_session_metadata(&js, "reservation-session").await?;
        Ok(Some(Self {
            worker,
            client,
            js,
            calls,
            subject,
            _env: env,
            _server: server,
        }))
    }

    async fn reserve(&self, selector: &str) -> Result<Reserved> {
        reserve(&self.client, &self.subject, "reservation-session", selector).await
    }

    async fn fs_registered(&self, reserved: &Reserved) -> Result<bool> {
        fs_registered(&self.js, reserved).await
    }

    async fn wait_for_fs_absent(&self, reserved: &Reserved) -> Result<()> {
        poll_until(async || Ok(!self.fs_registered(reserved).await?)).await
    }
}

async fn fs_registered(js: &async_nats::jetstream::Context, reserved: &Reserved) -> Result<bool> {
    let Ok(store) = js.get_key_value(TOOL_REGISTRY_BUCKET).await else {
        return Ok(false);
    };
    Ok(store
        .get(registration_key(
            &ServerScope::from_string(&reserved.server_scope),
            &harnx_toolset::server_identity_token(None, "fs", "fs"),
        ))
        .await?
        .is_some())
}

async fn reserve(
    client: &async_nats::Client,
    subject: &str,
    session: &str,
    selector: &str,
) -> Result<Reserved> {
    let request = Reserve::new(
        storage_key(session),
        ToolReservationView {
            package: None,
            use_tools: vec![selector.to_owned()],
        },
    );
    let reply = client
        .request(subject.to_owned(), serde_json::to_vec(&request)?.into())
        .await?;
    match serde_json::from_slice::<ReserveReply>(&reply.payload)? {
        ReserveReply::Reserved(reserved) => {
            assert_eq!(reserved.attempt_id, request.attempt_id);
            Ok(reserved)
        }
        ReserveReply::Error(error) => anyhow::bail!("reserve failed: {error:?}"),
    }
}

async fn control(
    client: &async_nats::Client,
    reserved: &Reserved,
    renew: bool,
) -> Result<ToolReservationControlReply> {
    let request = if renew {
        ToolReservationControl::Renew(Renew {
            reservation_id: reserved.reservation_id.clone(),
        })
    } else {
        ToolReservationControl::Release(Release {
            reservation_id: reserved.reservation_id.clone(),
        })
    };
    let reply = client
        .request(
            reserved.control_subject.clone(),
            serde_json::to_vec(&request)?.into(),
        )
        .await?;
    Ok(serde_json::from_slice(&reply.payload)?)
}

fn assert_ok(reply: ToolReservationControlReply) {
    assert_eq!(
        reply,
        ToolReservationControlReply::Ok(ToolReservationOk::Ok)
    );
}

async fn assert_expired_controls(client: &async_nats::Client, reserved: &Reserved) -> Result<()> {
    assert_expired(control(client, reserved, true).await?);
    assert_ok(control(client, reserved, false).await?);
    Ok(())
}

async fn assert_active_controls(client: &async_nats::Client, reserved: &Reserved) -> Result<()> {
    assert_ok(control(client, reserved, true).await?);
    assert_ok(control(client, reserved, false).await?);
    Ok(())
}

fn assert_expired(reply: ToolReservationControlReply) {
    assert!(
        matches!(
            reply,
            ToolReservationControlReply::Error(ToolReservationError {
                code: ToolReservationErrorCode::UnknownOrExpired,
                ..
            })
        ),
        "{reply:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reserve_without_model_starts_fs_and_returns_scope() -> Result<()> {
    let Some(f) = Fixture::start(Duration::from_secs(60), true, false).await? else {
        return Ok(());
    };
    let reserved = f.reserve("fs_*").await?;
    assert!(!reserved.server_scope.is_empty());
    assert_eq!(reserved.worker_id, "reservation-worker");
    assert_eq!(reserved.ttl_ms, 60_000);
    assert_eq!(reserved.renew_after_ms, 20_000);
    assert!(f.fs_registered(&reserved).await?);
    assert_eq!(
        f.calls.load(Ordering::SeqCst),
        0,
        "reserve must not call a model"
    );
    let bucket = open_lease_bucket(&f.js, &NatsLeaseConfig::default())
        .await
        .context("execution lease bucket")?;
    assert!(lease_holder_in(
        &bucket,
        &NatsLeaseConfig::default(),
        &storage_key("reservation-session")
    )
    .await?
    .is_none());
    assert!(!f.worker.is_finished());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn claim_survives_ordinary_activation_completion() -> Result<()> {
    let Some(f) = Fixture::start(Duration::from_secs(60), false, false).await? else {
        return Ok(());
    };
    let reserved = f.reserve("fs_*").await?;
    let log =
        NatsSessionLog::new_with_replicas(f.js.clone(), storage_key("reservation-session"), 1);
    log.append_event_async(&append_user_message_entry("reservation-turn", "hello"))
        .await?;
    publish_session_activate(
        &f.js,
        "local",
        &SessionActivate::new(storage_key("reservation-session")),
        1,
    )
    .await?;
    wait_until(CI_SAFE_TIMEOUT, || f.calls.load(Ordering::SeqCst) == 1).await?;
    wait_for_worker_session_cleanup(&f.js, "reservation-session").await?;
    assert!(log.load_events_async().await?.iter().any(|entry| matches!(
        &entry.1,
        SessionLogEntry::Message { role: MessageRole::Assistant, content: harnx_core::message::MessageContent::Text(text), .. } if text == "done"
    )), "ordinary activation must persist its final response");
    // Zero test linger means accidental turn cleanup of the reservation token
    // would deregister fs immediately, not leave a misleading running process.
    assert!(f.fs_registered(&reserved).await?);
    assert_ok(control(&f.client, &reserved, true).await?);
    assert_ok(control(&f.client, &reserved, false).await?);
    f.wait_for_fs_absent(&reserved).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn release_frees_claim_and_is_idempotent() -> Result<()> {
    let Some(f) = Fixture::start(Duration::from_secs(60), false, false).await? else {
        return Ok(());
    };
    let first = f.reserve("fs_*").await?;
    let second = f.reserve("fs_*").await?;
    let mut wrong_subject = second.clone();
    wrong_subject.control_subject = first.control_subject.clone();
    assert_expired(control(&f.client, &wrong_subject, true).await?);
    assert_ok(control(&f.client, &wrong_subject, false).await?);
    assert_ok(control(&f.client, &second, true).await?);
    assert_ne!(first.reservation_id, second.reservation_id);
    assert_ne!(first.control_subject, second.control_subject);
    assert_ok(control(&f.client, &first, false).await?);
    assert_ok(control(&f.client, &first, false).await?);
    assert_expired(control(&f.client, &first, true).await?);
    assert!(
        f.fs_registered(&second).await?,
        "other reservation still holds fs"
    );
    assert_ok(control(&f.client, &second, false).await?);
    f.wait_for_fs_absent(&second).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ttl_expiry_without_renew_frees_claim() -> Result<()> {
    let Some(f) = Fixture::start(Duration::from_secs(2), false, false).await? else {
        return Ok(());
    };
    let reserved = f.reserve("fs_*").await?;
    assert!(f.fs_registered(&reserved).await?);
    f.wait_for_fs_absent(&reserved).await?;
    assert_expired_controls(&f.client, &reserved).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn selectors_matching_nothing_succeed_without_servers() -> Result<()> {
    let Some(f) = Fixture::start(Duration::from_secs(60), false, false).await? else {
        return Ok(());
    };
    let reserved = f.reserve("does_not_exist_*").await?;
    assert!(!f.fs_registered(&reserved).await?);
    assert_active_controls(&f.client, &reserved).await?;
    assert_eq!(f.calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shared_queue_answers_once_and_controls_reach_winner() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let mut workers = WorkerPair::spawn(server.url(), &NatsLeaseConfig::default());
    workers.wait_until_ready().await?;
    let client = async_nats::connect(server.url()).await?;
    seed_session_metadata(
        &async_nats::jetstream::new(client.clone()),
        "shared-reservation",
    )
    .await?;
    let request = Reserve::new(
        storage_key("shared-reservation"),
        ToolReservationView::default(),
    );
    let inbox = client.new_inbox();
    let mut replies = client.subscribe(inbox.clone()).await?;
    client.flush().await?;
    client
        .publish_with_reply(
            reserve_subject("local"),
            inbox,
            serde_json::to_vec(&request)?.into(),
        )
        .await?;
    let message = tokio::time::timeout(CI_SAFE_TIMEOUT, replies.next())
        .await?
        .context("reserve reply")?;
    let ReserveReply::Reserved(reserved) = serde_json::from_slice(&message.payload)? else {
        anyhow::bail!("reserve error");
    };
    assert!(matches!(
        reserved.worker_id.as_str(),
        "worker-one" | "worker-two"
    ));
    assert_eq!(reserved.attempt_id, request.attempt_id);
    assert!(
        tokio::time::timeout(Duration::from_secs(1), replies.next())
            .await
            .is_err(),
        "more than one worker answered"
    );
    assert_ok(control(&client, &reserved, true).await?);
    assert_ok(control(&client, &reserved, false).await?);
    assert_expired(control(&client, &reserved, true).await?);
    workers.assert_running();
    workers.abort_and_assert_cancelled().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn killed_worker_renew_fails_and_rereserve_changes_scope() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let mut workers = WorkerPair::spawn(server.url(), &NatsLeaseConfig::default());
    workers.wait_until_ready().await?;
    let client = async_nats::connect(server.url()).await?;
    seed_session_metadata(
        &async_nats::jetstream::new(client.clone()),
        "failover-reservation",
    )
    .await?;
    let first = reserve(
        &client,
        &reserve_subject("local"),
        "failover-reservation",
        "nothing_*",
    )
    .await?;
    let winner = if first.worker_id == "worker-one" {
        &mut workers.worker_one
    } else {
        &mut workers.worker_two
    };
    winner.abort();
    assert!(winner.await.unwrap_err().is_cancelled());
    poll_until(async || Ok(control(&client, &first, true).await.is_err())).await?;
    let second = reserve(
        &client,
        &reserve_subject("local"),
        "failover-reservation",
        "nothing_*",
    )
    .await?;
    assert_ne!(first.worker_id, second.worker_id);
    assert_ne!(first.server_scope, second.server_scope);
    assert_ne!(first.control_subject, second.control_subject);
    assert_ne!(first.attempt_id, second.attempt_id);
    assert_ok(control(&client, &second, true).await?);
    assert_ok(control(&client, &second, false).await?);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unmanaged_reserve_returns_configured_scope_without_launching() -> Result<()> {
    let Some(f) = Fixture::start(Duration::from_secs(60), false, true).await? else {
        return Ok(());
    };
    let reserved = f.reserve("fs_*").await?;
    assert_eq!(reserved.server_scope, "unmanaged-reservation-test");
    assert!(!f.fs_registered(&reserved).await?);
    assert_active_controls(&f.client, &reserved).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invalid_request_or_missing_metadata_does_not_claim_servers() -> Result<()> {
    let Some(f) = Fixture::start(Duration::from_secs(60), false, false).await? else {
        return Ok(());
    };
    let view = ToolReservationView {
        package: None,
        use_tools: vec!["fs_*".to_owned()],
    };
    let mut bad_version = Reserve::new(storage_key("reservation-session"), view.clone());
    bad_version.protocol_version += 1;
    for request in [
        bad_version,
        Reserve::new(storage_key("missing-session"), view),
    ] {
        let reply = f
            .client
            .request(f.subject.clone(), serde_json::to_vec(&request)?.into())
            .await?;
        assert!(matches!(
            serde_json::from_slice::<ReserveReply>(&reply.payload)?,
            ReserveReply::Error(_)
        ));
    }
    assert_eq!(f.calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn renew_extends_expiry_past_original_ttl() -> Result<()> {
    let Some(f) = Fixture::start(Duration::from_secs(4), false, false).await? else {
        return Ok(());
    };
    let reserved = f.reserve("nothing_*").await?;
    for _ in 0..5 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_ok(control(&f.client, &reserved, true).await?);
    }
    assert_ok(control(&f.client, &reserved, false).await?);
    Ok(())
}
