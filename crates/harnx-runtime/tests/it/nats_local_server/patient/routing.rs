//! T2 frontend notice-routing tests: transfer to the live sink without a
//! replayed backlog, and routing cleanup on scope shutdown.

use super::*;

/// The frontend's broker-notice route transfers to the sink the live frontend
/// owns (design 9): future notices arrive only at the new sink, the bootstrap
/// sink gets no replayed backlog, and after the frontend scope shuts down no
/// sink receives anything.
#[tokio::test]
async fn notice_route_transfers_without_backlog_and_stops_after_shutdown() -> Result<()> {
    let Some(gate) = Gate::new()? else {
        return Ok(());
    };
    // Shrink the notice cadence so the test observes routing, not wall-clock
    // patience; the seam never changes production deadlines.
    let _seam = crate::worker::EnvGuard::set("HARNX_TEST_BROKER_NOTICE_MS", "200");
    let (mut scope, mut bootstrap) = scope()?;
    let config = global();
    let reserve = Config::reserve_new_session_id(&config);
    tokio::pin!(reserve);
    // Polling the reservation starts the shared broker acquisition.
    assert!(poll!(&mut reserve).is_pending());
    let pid = gate.started(0).await?;

    let first_notice = next_notice(&mut bootstrap).await?;
    assert!(
        first_notice.contains(&format!("PID {pid}")),
        "bootstrap route must carry the owned PID: {first_notice}"
    );

    // Transfer to the live-frontend sink: the next notice arrives only there.
    let (live_tx, mut live) = mpsc::unbounded_channel();
    scope.set_notice_sink(Arc::new(move |notice| {
        let _ = live_tx.send(notice);
    }));
    let transferred = next_notice(&mut live).await?;
    assert!(
        transferred.contains(&format!("PID {pid}")),
        "live route must keep the observed phase and PID: {transferred}"
    );
    assert!(
        bootstrap.try_recv().is_err(),
        "transfer must not replay a backlog to the bootstrap sink"
    );

    // A wait-only cancellation keeps the shared producer, so notices keep
    // flowing to the current sink after one caller aborts.
    let abort = create_abort_signal();
    let session = NatsSession::from_global_config(session_config(), &config, abort.clone());
    tokio::pin!(session);
    assert!(poll!(&mut session).is_pending());
    abort.set_ctrlc();
    let error = within(session)
        .await
        .err()
        .context("cancelled waiter returned ready")?;
    assert!(format!("{error:#}").contains("cancelled while waiting for NATS"));
    assert!(
        next_notice(&mut live)
            .await?
            .contains(&format!("PID {pid}")),
        "waiter cancellation must not stop the shared notice producer"
    );

    // Scope shutdown stops routing entirely: the reaped child cannot produce
    // more notices, and no sink receives anything after it returns.
    scope.shutdown().await?;
    assert_reaped(pid);
    gate.allow()?;
    sleep(Duration::from_millis(300)).await;
    assert!(
        live.try_recv().is_err(),
        "routing must stop once the frontend scope has shut down"
    );
    assert!(
        bootstrap.try_recv().is_err(),
        "bootstrap route must not be restored after shutdown"
    );
    Ok(())
}

/// The TUI's bootstrap-failure route: reroute to the live sink, then restore
/// the exact previous sink when the replacement route is about to disappear
/// (terminal setup error). After the restore, notices flow only to the
/// restored sink — the dead frontend sink gets nothing.
#[tokio::test]
async fn notice_reroute_restore_returns_the_previous_sink() -> Result<()> {
    use harnx_runtime::nats_local_server::{
        reroute_frontend_broker_notices, restore_frontend_broker_notices,
    };

    let Some(gate) = Gate::new()? else {
        return Ok(());
    };
    let _seam = crate::worker::EnvGuard::set("HARNX_TEST_BROKER_NOTICE_MS", "200");
    let (mut scope, mut bootstrap) = scope()?;
    let config = global();
    let reserve = Config::reserve_new_session_id(&config);
    tokio::pin!(reserve);
    // Polling the reservation starts the shared broker acquisition.
    assert!(poll!(&mut reserve).is_pending());
    let pid = gate.started(0).await?;

    // What Tui::run does before terminal setup: move the route to the live
    // frontend sink. The previous sink is returned for the failure path.
    let (live_tx, mut live) = mpsc::unbounded_channel();
    let previous = reroute_frontend_broker_notices(Arc::new(move |notice| {
        let _ = live_tx.send(notice);
    }))?;
    assert!(
        previous.is_some(),
        "the scope's bootstrap sink must have been installed"
    );
    let transferred = next_notice(&mut live).await?;
    assert!(
        transferred.contains(&format!("PID {pid}")),
        "live route must carry the owned PID: {transferred}"
    );

    // What the terminal-setup failure path does: restore the exact previous
    // sink. Notices must now flow to the restored route only.
    restore_frontend_broker_notices(previous);
    let restored = next_notice(&mut bootstrap).await?;
    assert!(
        restored.contains(&format!("PID {pid}")),
        "restored route must carry the owned PID: {restored}"
    );
    assert!(
        live.try_recv().is_err(),
        "the failed frontend's sink must not receive notices after the restore"
    );

    scope.shutdown().await?;
    assert_reaped(pid);
    gate.allow()?;
    Ok(())
}
