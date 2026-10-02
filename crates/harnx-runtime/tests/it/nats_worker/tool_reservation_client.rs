use super::support::WorkerPair;
use super::*;
use anyhow::{ensure, Context};
use futures_util::StreamExt;
use harnx_core::abort::create_abort_signal;
use harnx_core::instance::ServerScope;
use harnx_core::tool::ToolProvider;
use harnx_runtime::config::{NatsRouting, ToolServerConfig};
use harnx_runtime::nats_session_metadata::SessionAgentSource;
use harnx_runtime::nats_tool_provider::{NatsInFlightCalls, NatsToolProvider};
use harnx_runtime::nats_worker::tool_reservation::*;
use harnx_runtime::tool_reservation_client::ToolReservationHandle;
use harnx_runtime::SessionActivationRoute;
use harnx_toolset_server::{registration_key, TOOL_REGISTRY_BUCKET};
use serde_json::json;
use tokio_util::task::AbortOnDropHandle;

fn view(selectors: &[&str]) -> ToolReservationView {
    ToolReservationView {
        package: None,
        use_tools: selectors.iter().map(|s| (*s).to_owned()).collect(),
    }
}

fn server_config(name: &str, bin: &str) -> Result<ToolServerConfig> {
    let binary = std::env::current_exe()?
        .parent()
        .context("test binary dir")?
        .parent()
        .context("target debug dir")?
        .join(format!("{bin}{}", std::env::consts::EXE_SUFFIX));
    ensure!(
        binary.is_file(),
        "build workspace first: {} is missing",
        binary.display()
    );
    Ok(ToolServerConfig {
        name: name.to_owned(),
        command: binary.to_string_lossy().into_owned(),
        args: vec!["--name".to_owned(), name.to_owned()],
        env: Default::default(),
        enabled: true,
        description: None,
        package: None,
        hooks: None,
    })
}

async fn wait_for_empty_scope(client: &async_nats::Client, scope: &ServerScope) -> Result<()> {
    let store = async_nats::jetstream::new(client.clone())
        .get_key_value(TOOL_REGISTRY_BUCKET)
        .await?;
    poll_until(async || {
        for name in ["fs", "attachments"] {
            let key = registration_key(
                scope,
                &harnx_toolset::server_identity_token(None, name, name),
            );
            if store.get(key).await?.is_some() {
                return Ok(false);
            }
        }
        Ok(true)
    })
    .await
}

struct LocalWorkerHarness {
    _worker: AbortOnDropHandle<Result<()>>,
    config: Config,
    route: SessionActivationRoute,
    _env: [EnvGuard; 3],
}

impl LocalWorkerHarness {
    async fn spawn(server_url: &str) -> Result<Self> {
        let env = [
            EnvGuard::set("HARNX_NATS_URL", server_url),
            EnvGuard::set("HARNX_NATS_TOKEN", ""),
            // open must not overwrite caller-supplied routing from frontend env.
            EnvGuard::set("HARNX_NATS_SERVER", "not-the-selected-cluster"),
        ];
        let global = local_nats_runtime_config(server_url);
        {
            let mut config = global.write();
            config.model = Default::default();
            config.clients.clear();
            config.tool_servers = vec![
                server_config("fs", "harnx-fs-tools")?,
                server_config("attachments", "harnx-attachment-tools")?,
            ];
        }
        let readiness = harnx_healthz::Readiness::default();
        let daemon = WorkerDaemonConfig::local("client-worker")?
            .with_tool_reservation_timing_for_test(Duration::from_secs(3), Duration::ZERO);
        let mut worker = AbortOnDropHandle::new(tokio::spawn({
            let readiness = readiness.clone();
            let global = global.clone();
            async move { run_worker_daemon(global, daemon, None, Some(readiness)).await }
        }));
        tokio::select! {
            result = wait_until(CI_SAFE_TIMEOUT, || readiness.is_ready()) => result?,
            result = &mut worker => anyhow::bail!("client worker stopped: {result:?}"),
        }
        let config = global.read().clone();
        let route = SessionActivationRoute::WorkerTargeted {
            session_scope: "__local__".to_owned(),
            worker_id: "client-worker".to_owned(),
        };
        Ok(Self {
            _env: env,
            _worker: worker,
            config,
            route,
        })
    }
}

async fn verify_caller_identity_tool_call(
    handle: &ToolReservationHandle,
    scope: &ServerScope,
) -> Result<()> {
    let provider = NatsToolProvider::discover(
        handle.config(),
        scope.clone(),
        NatsInFlightCalls::for_instance(scope),
        None,
    )
    .await?;
    assert!(provider
        .declarations()
        .iter()
        .any(|tool| tool.name == "fs_read"));
    let result = provider
        .call_tool(
            "attachments_attachment_create",
            json!({"content": "reservation caller identity", "mime_type": "text/plain"}),
            &create_abort_signal(),
        )
        .await
        .map_err(|error| match error {
            harnx_core::tool::ToolError::Recoverable(error)
            | harnx_core::tool::ToolError::Fatal(error) => error,
        })?;
    assert!(!result.value.to_string().contains("error"), "{result:?}");
    assert!(result.value.to_string().contains("cid:"), "{result:?}");
    Ok(())
}

async fn assert_inline_metadata(
    client: &async_nats::Client,
    key: &str,
) -> Result<SessionMetadataStore> {
    let metadata =
        SessionMetadataStore::ensure(&async_nats::jetstream::new(client.clone()), 1).await?;
    assert!(matches!(
        metadata
            .get(key)
            .await?
            .context("backing metadata missing")?
            .metadata
            .agent,
        SessionAgentSource::Inline { .. }
    ));
    Ok(metadata)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_open_discovers_tools_with_caller_identity_renews_and_closes() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let harness = LocalWorkerHarness::spawn(server.url()).await?;
    let mut handle = ToolReservationHandle::open(
        harness.config.clone(),
        harness.route.clone(),
        view(&["fs_*", "attachments_*"]),
    )
    .await?;
    assert!(
        harness.config.session.is_none(),
        "process-wide config was modified"
    );
    assert_eq!(handle.generation(), 0);
    assert_eq!(
        handle.config().session.as_ref().unwrap().storage_key(),
        handle.session_storage_key()
    );
    let scope = handle.server_scope().context("initial scope unavailable")?;
    verify_caller_identity_tool_call(&handle, &scope).await?;

    let client = async_nats::connect(server.url()).await?;
    let id = handle.session_id().to_owned();
    let key = handle.session_storage_key().to_owned();
    let metadata = assert_inline_metadata(&client, &key).await?;

    // Keep the claim beyond its original TTL. Successful renews don't bump generation.
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(handle.generation(), 0);
    assert_eq!(handle.server_scope(), Some(scope.clone()));
    handle.close().await?;
    handle.close().await?;
    assert!(handle.server_scope().is_none());
    wait_for_empty_scope(&client, &scope).await?;
    assert!(
        metadata.get(&key).await?.is_some(),
        "close deleted backing session"
    );

    // A second connection gets a distinct session; Drop releases without waiting for TTL.
    let dropped =
        ToolReservationHandle::open(harness.config, harness.route, view(&["fs_*"])).await?;
    assert_ne!(dropped.session_id(), id);
    let dropped_scope = dropped.server_scope().context("drop scope missing")?;
    drop(dropped);
    wait_for_empty_scope(&client, &dropped_scope).await?;
    Ok(())
}

async fn await_matching_reserve_reply(
    replies: &mut async_nats::Subscriber,
    expected_scope: &str,
) -> Result<Reserved> {
    tokio::time::timeout(CI_SAFE_TIMEOUT, async {
        loop {
            let message = replies.next().await.context("reply observer closed")?;
            match serde_json::from_slice::<ReserveReply>(&message.payload) {
                Ok(ReserveReply::Reserved(reply)) if reply.server_scope == expected_scope => {
                    return Ok::<_, anyhow::Error>(reply);
                }
                _ => {}
            }
        }
    })
    .await?
}

async fn abort_winning_worker(workers: &mut WorkerPair, winner_worker_id: &str) {
    let winner = if winner_worker_id == "worker-one" {
        &mut workers.worker_one
    } else {
        &mut workers.worker_two
    };
    winner.abort();
    assert!(winner.await.unwrap_err().is_cancelled());
}

async fn verify_failover_discovery(
    handle: &ToolReservationHandle,
    scope: &ServerScope,
) -> Result<()> {
    let provider = NatsToolProvider::discover(
        handle.config(),
        scope.clone(),
        NatsInFlightCalls::for_instance(scope),
        None,
    )
    .await?;
    assert!(provider.declarations().is_empty());
    assert_eq!(
        handle.config().nats_routing,
        NatsRouting::Cluster("local".to_owned())
    );
    Ok(())
}

async fn await_failover_replacement(
    handle: &ToolReservationHandle,
    first_scope: &ServerScope,
) -> Result<ServerScope> {
    let mut changes = handle.subscribe();
    let lost = tokio::time::timeout(
        CI_SAFE_TIMEOUT,
        changes.wait_for(|state| state.server_scope.is_none()),
    )
    .await??;
    assert!(lost.generation > 0);
    drop(lost);
    let replacement = tokio::time::timeout(
        CI_SAFE_TIMEOUT,
        changes.wait_for(|state| {
            state
                .server_scope
                .as_ref()
                .is_some_and(|scope| scope != first_scope)
        }),
    )
    .await??;
    assert!(replacement.generation >= 2);
    let second_scope = replacement.server_scope.clone().unwrap();
    Ok(second_scope)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shared_worker_loss_invalidates_scope_and_re_reserves_on_other_worker() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let _env = [
        EnvGuard::set("HARNX_NATS_URL", server.url()),
        EnvGuard::set("HARNX_NATS_TOKEN", ""),
        EnvGuard::set("HARNX_NATS_SERVER", "wrong-env-cluster"),
    ];
    let mut workers = WorkerPair::spawn(server.url(), &short_lease_config());
    workers.wait_until_ready().await?;
    let client = async_nats::connect(server.url()).await?;
    let mut replies = client.subscribe("_INBOX.>").await?;
    client.flush().await?;
    let mut config = local_nats_runtime_config(server.url()).read().clone();
    config.nats_routing = NatsRouting::Cluster("local".to_owned());
    let mut handle = ToolReservationHandle::open(
        config,
        SessionActivationRoute::ClusterShared,
        view(&["nothing_*"]),
    )
    .await?;
    let first_scope = handle.server_scope().context("first scope missing")?;
    let first = await_matching_reserve_reply(&mut replies, first_scope.as_str()).await?;
    replies.unsubscribe().await?;
    client.flush().await?;

    abort_winning_worker(&mut workers, &first.worker_id).await;
    let second_scope = await_failover_replacement(&handle, &first_scope).await?;
    verify_failover_discovery(&handle, &second_scope).await?;
    handle.close().await?;
    Ok(())
}

#[derive(Clone, Copy)]
enum RenewFailure {
    Expired,
    Timeout,
    NoResponders,
}

struct ScriptedRecoveryPeer {
    client: async_nats::Client,
    requests: async_nats::Subscriber,
    first_control: String,
    next_control: String,
    controls: async_nats::Subscriber,
    next_controls: async_nats::Subscriber,
}

impl ScriptedRecoveryPeer {
    async fn setup(client: &async_nats::Client) -> Result<Self> {
        let requests = client.subscribe(reserve_subject("local")).await?;
        let first_control = client.new_inbox();
        let next_control = client.new_inbox();
        let controls = client.subscribe(first_control.clone()).await?;
        let next_controls = client.subscribe(next_control.clone()).await?;
        client.flush().await?;
        Ok(Self {
            client: client.clone(),
            requests,
            first_control,
            next_control,
            controls,
            next_controls,
        })
    }

    async fn run_mock_worker(mut self, failure: RenewFailure) -> Result<()> {
        let first = self
            .requests
            .next()
            .await
            .context("first reserve missing")?;
        let request: Reserve = serde_json::from_slice(&first.payload)?;
        let reply = Reserved {
            protocol_version: TOOL_RESERVATION_PROTOCOL_VERSION,
            attempt_id: request.attempt_id.clone(),
            reservation_id: "first".to_owned(),
            worker_id: "mock-worker".to_owned(),
            server_scope: "same-scope".to_owned(),
            control_subject: self.first_control.clone(),
            ttl_ms: 60_000,
            renew_after_ms: 100,
        };
        self.client
            .publish(
                first.reply.context("reserve reply missing")?,
                serde_json::to_vec(&reply)?.into(),
            )
            .await?;

        Self::simulate_renew_failure(&self.client, failure, &mut self.controls).await?;

        let second = self
            .requests
            .next()
            .await
            .context("second reserve missing")?;
        let retry: Reserve = serde_json::from_slice(&second.payload)?;
        assert_ne!(request.attempt_id, retry.attempt_id);
        assert_eq!(request.session_storage_key, retry.session_storage_key);
        assert_eq!(request.view, retry.view);
        let replacement = Reserved {
            attempt_id: retry.attempt_id,
            reservation_id: "second".to_owned(),
            control_subject: self.next_control.clone(),
            renew_after_ms: 20_000,
            ..reply
        };
        self.client
            .publish(
                second.reply.context("retry reply missing")?,
                serde_json::to_vec(&replacement)?.into(),
            )
            .await?;

        self.handle_releases_until_complete(failure).await
    }

    async fn simulate_renew_failure(
        client: &async_nats::Client,
        failure: RenewFailure,
        controls: &mut async_nats::Subscriber,
    ) -> Result<()> {
        match failure {
            RenewFailure::NoResponders => {
                controls.unsubscribe().await?;
                client.flush().await?;
            }
            RenewFailure::Expired | RenewFailure::Timeout => {
                let renew = controls.next().await.context("renew missing")?;
                assert!(matches!(
                    serde_json::from_slice::<ToolReservationControl>(&renew.payload)?,
                    ToolReservationControl::Renew(_)
                ));
                if matches!(failure, RenewFailure::Expired) {
                    client
                        .publish(
                            renew.reply.context("renew reply missing")?,
                            serde_json::to_vec(&ToolReservationControlReply::Error(
                                ToolReservationError {
                                    code: ToolReservationErrorCode::UnknownOrExpired,
                                    message: "expired".to_owned(),
                                },
                            ))?
                            .into(),
                        )
                        .await?;
                }
            }
        }
        Ok(())
    }

    async fn handle_releases_until_complete(&mut self, failure: RenewFailure) -> Result<()> {
        loop {
            let (message, current) = tokio::select! {
                message = self.controls.next(), if !matches!(failure, RenewFailure::NoResponders) => {
                    (message.context("old controls closed")?, false)
                }
                message = self.next_controls.next() => (message.context("new controls closed")?, true),
            };
            assert!(matches!(
                serde_json::from_slice::<ToolReservationControl>(&message.payload)?,
                ToolReservationControl::Release(_)
            ));
            self.client
                .publish(
                    message.reply.context("release reply missing")?,
                    serde_json::to_vec(&ToolReservationControlReply::Ok(ToolReservationOk::Ok))?
                        .into(),
                )
                .await?;
            if current {
                return Ok(());
            }
        }
    }
}

async fn scripted_recovery(failure: RenewFailure) -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let peer = ScriptedRecoveryPeer::setup(&client).await?;
    let mock = AbortOnDropHandle::new(tokio::spawn(peer.run_mock_worker(failure)));

    let mut config = local_nats_runtime_config(server.url()).read().clone();
    config.nats_routing = NatsRouting::Cluster("local".to_owned());
    let mut handle = ToolReservationHandle::open(
        config,
        SessionActivationRoute::ClusterShared,
        view(&["fs_*"]),
    )
    .await?;

    let mut changes = handle.subscribe();
    let unavailable = tokio::time::timeout(
        CI_SAFE_TIMEOUT,
        changes.wait_for(|state| state.server_scope.is_none()),
    )
    .await??;
    assert_eq!(unavailable.generation, 1);
    drop(unavailable);
    let ready = tokio::time::timeout(
        CI_SAFE_TIMEOUT,
        changes.wait_for(|state| state.generation >= 2),
    )
    .await??;
    assert_eq!(ready.server_scope.as_ref().unwrap().as_str(), "same-scope");
    drop(ready);
    handle.close().await?;
    tokio::time::timeout(CI_SAFE_TIMEOUT, mock).await???;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_or_expired_re_reserves_with_fresh_attempt_even_on_same_scope() -> Result<()> {
    scripted_recovery(RenewFailure::Expired).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn renew_timeout_re_reserves_with_fresh_attempt() -> Result<()> {
    scripted_recovery(RenewFailure::Timeout).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn renew_no_responders_re_reserves_with_fresh_attempt() -> Result<()> {
    scripted_recovery(RenewFailure::NoResponders).await
}

async fn spawn_in_flight_renew_worker(
    client: async_nats::Client,
    control_subject: String,
    renew_started: tokio::sync::oneshot::Sender<()>,
) -> Result<AbortOnDropHandle<Result<()>>> {
    let mut requests = client.subscribe(reserve_subject("local")).await?;
    let mut controls = client.subscribe(control_subject.clone()).await?;
    client.flush().await?;
    Ok(AbortOnDropHandle::new(tokio::spawn(async move {
        let message = requests.next().await.context("reserve missing")?;
        let request: Reserve = serde_json::from_slice(&message.payload)?;
        let reply = Reserved {
            protocol_version: TOOL_RESERVATION_PROTOCOL_VERSION,
            attempt_id: request.attempt_id,
            reservation_id: "shutdown-reservation".to_owned(),
            worker_id: "shutdown-worker".to_owned(),
            server_scope: "shutdown-scope".to_owned(),
            control_subject,
            ttl_ms: 60_000,
            renew_after_ms: 100,
        };
        client
            .publish(
                message.reply.context("reserve reply missing")?,
                serde_json::to_vec(&reply)?.into(),
            )
            .await?;
        let renew = controls.next().await.context("renew missing")?;
        assert!(matches!(
            serde_json::from_slice::<ToolReservationControl>(&renew.payload)?,
            ToolReservationControl::Renew(_)
        ));
        let _ = renew_started.send(());
        // Don't answer renew. Cleanup must cancel its waiter before releasing.
        let release = controls.next().await.context("release missing")?;
        assert!(matches!(
            serde_json::from_slice::<ToolReservationControl>(&release.payload)?,
            ToolReservationControl::Release(_)
        ));
        client
            .publish(
                release.reply.context("release reply missing")?,
                serde_json::to_vec(&ToolReservationControlReply::Ok(ToolReservationOk::Ok))?.into(),
            )
            .await?;
        client.flush().await?;
        Ok::<_, anyhow::Error>(())
    })))
}

async fn shutdown_during_renew(drop_handle: bool) -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let control_subject = client.new_inbox();
    let (renew_started, renewed) = tokio::sync::oneshot::channel();
    let mock = spawn_in_flight_renew_worker(client, control_subject, renew_started).await?;

    let mut config = local_nats_runtime_config(server.url()).read().clone();
    config.nats_routing = NatsRouting::Cluster("local".to_owned());
    let mut handle = ToolReservationHandle::open(
        config,
        SessionActivationRoute::ClusterShared,
        view(&["nothing_*"]),
    )
    .await?;
    let mut changes = handle.subscribe();
    tokio::time::timeout(CI_SAFE_TIMEOUT, renewed).await??;
    if drop_handle {
        drop(handle);
    } else {
        handle.close().await?;
        assert!(handle.server_scope().is_none());
    }
    tokio::time::timeout(CI_SAFE_TIMEOUT, mock).await???;
    let closed = tokio::time::timeout(
        CI_SAFE_TIMEOUT,
        changes.wait_for(|state| state.server_scope.is_none()),
    )
    .await??;
    assert_eq!(closed.generation, 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_stops_pending_renew_before_release() -> Result<()> {
    shutdown_during_renew(false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drop_stops_pending_renew_and_releases_best_effort() -> Result<()> {
    shutdown_during_renew(true).await
}

// Keep the protocol real but let each test control worker disappearance and replies.
async fn open_scripted_reservation(
    url: &str,
    renew_after_ms: u64,
) -> Result<(
    ToolReservationHandle,
    async_nats::Client,
    async_nats::Subscriber,
    String,
)> {
    let client = async_nats::connect(url).await?;
    let mut requests = client.subscribe(reserve_subject("local")).await?;
    let control_subject = client.new_inbox();
    let controls = client.subscribe(control_subject.clone()).await?;
    client.flush().await?;
    let worker = AbortOnDropHandle::new(tokio::spawn({
        let client = client.clone();
        let control_subject = control_subject.clone();
        async move {
            let message = requests.next().await.context("reserve missing")?;
            let request: Reserve = serde_json::from_slice(&message.payload)?;
            let reservation = Reserved {
                protocol_version: TOOL_RESERVATION_PROTOCOL_VERSION,
                attempt_id: request.attempt_id,
                reservation_id: "close-reservation".to_owned(),
                worker_id: "close-worker".to_owned(),
                server_scope: "close-scope".to_owned(),
                control_subject,
                ttl_ms: 600_000,
                renew_after_ms,
            };
            client
                .publish(
                    message.reply.context("reserve reply missing")?,
                    serde_json::to_vec(&reservation)?.into(),
                )
                .await?;
            requests.unsubscribe().await?;
            client.flush().await?;
            Ok::<_, anyhow::Error>(())
        }
    }));
    let mut config = local_nats_runtime_config(url).read().clone();
    config.nats_routing = NatsRouting::Cluster("local".to_owned());
    let handle = ToolReservationHandle::open(
        config,
        SessionActivationRoute::ClusterShared,
        view(&["nothing_*"]),
    )
    .await?;
    tokio::time::timeout(CI_SAFE_TIMEOUT, worker).await???;
    Ok((handle, client, controls, control_subject))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_invalidated_reservation_does_not_send_release() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let (mut handle, client, mut controls, control_subject) =
        open_scripted_reservation(server.url(), 100).await?;
    // Removing both worker subscriptions makes renewal/recovery see no responders.
    controls.unsubscribe().await?;
    client.flush().await?;
    let mut changes = handle.subscribe();
    let invalidated = tokio::time::timeout(
        CI_SAFE_TIMEOUT,
        changes.wait_for(|state| state.server_scope.is_none()),
    )
    .await??;
    assert_eq!(invalidated.generation, 1);
    drop(invalidated);

    // Observe the old control subject only after renewal has failed. A sentinel
    // checks absence of release without a sleep or a short negative deadline.
    let mut observer = client.subscribe(control_subject.clone()).await?;
    client.flush().await?;
    tokio::time::timeout(CI_SAFE_TIMEOUT, handle.close()).await??;
    assert!(handle.server_scope().is_none());
    assert_eq!(handle.generation(), 2);
    handle.close().await?;
    assert_eq!(handle.generation(), 2, "close must be idempotent");
    client
        .publish(control_subject, "after-close".into())
        .await?;
    client.flush().await?;
    let message = tokio::time::timeout(CI_SAFE_TIMEOUT, observer.next())
        .await?
        .context("close sentinel missing")?;
    assert_eq!(
        message.payload.as_ref(),
        b"after-close",
        "invalidated reservation was released"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_ready_release_no_responders_is_best_effort() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let (mut handle, client, mut controls, _) =
        open_scripted_reservation(server.url(), 300_000).await?;
    controls.unsubscribe().await?;
    client.flush().await?;
    assert!(handle.server_scope().is_some());
    assert_eq!(handle.generation(), 0);
    tokio::time::timeout(CI_SAFE_TIMEOUT, handle.close()).await??;
    assert!(handle.server_scope().is_none());
    assert_eq!(handle.generation(), 1);
    handle.close().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_ready_release_timeout_is_best_effort() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let (mut handle, _client, mut controls, _) =
        open_scripted_reservation(server.url(), 300_000).await?;
    assert!(handle.server_scope().is_some());
    assert_eq!(handle.generation(), 0);
    // Leave the control subscriber alive but never reply to the release request.
    tokio::time::timeout(CI_SAFE_TIMEOUT, handle.close()).await??;
    let message = tokio::time::timeout(CI_SAFE_TIMEOUT, controls.next())
        .await?
        .context("release missing")?;
    let operation: ToolReservationControl = serde_json::from_slice(&message.payload)?;
    assert!(
        matches!(operation, ToolReservationControl::Release(Release { reservation_id }) if reservation_id == "close-reservation")
    );
    assert!(handle.server_scope().is_none());
    assert_eq!(handle.generation(), 1);
    handle.close().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_invalidates_scope_then_waits_for_release_acknowledgement() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let (mut handle, client, mut controls, _) =
        open_scripted_reservation(server.url(), 300_000).await?;
    let changes = handle.subscribe();
    {
        let close = handle.close();
        tokio::pin!(close);
        let release = tokio::time::timeout(CI_SAFE_TIMEOUT, async {
            tokio::select! {
                biased;
                result = &mut close => anyhow::bail!("close finished before release acknowledgement: {result:?}"),
                message = controls.next() => message.context("release missing"),
            }
        })
        .await??;
        assert!(matches!(
            serde_json::from_slice::<ToolReservationControl>(&release.payload)?,
            ToolReservationControl::Release(Release { reservation_id }) if reservation_id == "close-reservation"
        ));
        assert!(
            changes.borrow().server_scope.is_none(),
            "scope must be invalidated before release"
        );
        assert!(
            futures_util::poll!(close.as_mut()).is_pending(),
            "close didn't wait for release reply"
        );
        client
            .publish(
                release.reply.context("release reply missing")?,
                serde_json::to_vec(&ToolReservationControlReply::Ok(ToolReservationOk::Ok))?.into(),
            )
            .await?;
        client.flush().await?;
        tokio::time::timeout(CI_SAFE_TIMEOUT, close).await??;
    }
    assert_eq!(handle.generation(), 1);
    handle.close().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn initial_reserve_no_responders_still_fails_open() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let mut config = local_nats_runtime_config(server.url()).read().clone();
    config.nats_routing = NatsRouting::Cluster("local".to_owned());
    let result = tokio::time::timeout(
        CI_SAFE_TIMEOUT,
        ToolReservationHandle::open(
            config,
            SessionActivationRoute::ClusterShared,
            view(&["nothing_*"]),
        ),
    )
    .await?;
    let error = result
        .err()
        .context("initial reserve unexpectedly succeeded")?;
    assert!(
        error
            .to_string()
            .contains("tool reservation reserve request failed"),
        "{error:#}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_after_dead_worker_is_best_effort() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let global = local_nats_runtime_config(server.url());
    let readiness = harnx_healthz::Readiness::default();
    let daemon = WorkerDaemonConfig::local("close-worker")?
        .with_tool_reservation_timing_for_test(Duration::from_secs(3), Duration::ZERO);
    let mut worker = AbortOnDropHandle::new(tokio::spawn({
        let global = global.clone();
        let readiness = readiness.clone();
        async move { run_worker_daemon(global, daemon, None, Some(readiness)).await }
    }));
    tokio::select! {
        result = wait_until(CI_SAFE_TIMEOUT, || readiness.is_ready()) => result?,
        result = &mut worker => anyhow::bail!("close worker stopped: {result:?}"),
    }
    let config = global.read().clone();
    let mut handle = ToolReservationHandle::open(
        config,
        SessionActivationRoute::WorkerTargeted {
            session_scope: "__local__".to_owned(),
            worker_id: "close-worker".to_owned(),
        },
        view(&["nothing_*"]),
    )
    .await?;
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    let mut changes = handle.subscribe();
    let invalidated = tokio::time::timeout(
        CI_SAFE_TIMEOUT,
        changes.wait_for(|state| state.server_scope.is_none()),
    )
    .await??;
    assert_eq!(invalidated.generation, 1);
    drop(invalidated);
    tokio::time::timeout(CI_SAFE_TIMEOUT, handle.close()).await??;
    assert!(handle.server_scope().is_none());
    assert_eq!(handle.generation(), 2);
    handle.close().await?;
    Ok(())
}
