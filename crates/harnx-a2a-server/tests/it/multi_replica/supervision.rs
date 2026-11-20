use super::admission::{remove_owner_lease, reservation};
use super::*;
use a2a_lf::TaskState;
use harnx_a2a_server::store::FirstMessageReservation;

pub(super) fn supervise(t: &TwoBackends, replica: &Replica) {
    replica
        .backend
        .runner
        .start_supervision(harnx_a2a_server::runner::SupervisionConfig {
            exports: vec![t.h.export.clone()],
            config: t.h.config.clone(),
            route: SessionActivationRoute::ClusterShared,
            abort: replica.backend.abort.clone(),
        });
}
async fn session_for(replica: &Replica, h: &Harness, local: &str) -> Result<NatsSession> {
    replica
        .backend
        .runner
        .session(SessionRequest {
            export: &h.export,
            owner: &alice().into(),
            local_id: Some(local),
            global_config: &h.config,
            activation_route: SessionActivationRoute::ClusterShared,
            abort: replica.backend.abort.clone(),
        })
        .await
}
pub(super) async fn terminal(
    t: &TwoBackends,
    id: &str,
) -> Result<harnx_a2a_server::store::TaskRecord> {
    tokio::time::timeout(DEADLINE, async {
        loop {
            if let Some(task) =
                t.b.backend
                    .store
                    .get_task_for_export(&t.h.export, &alice().into(), id)
                    .await?
            {
                if task.task.status.state.is_terminal() {
                    return Ok(task);
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .context("background terminal deadline")?
}

/// The shared fault-injection prefix of first-message recovery tests: pause
/// replica A's admission at `Boundary::FirstReservation`, keep the
/// dispatching request in flight, and read back the reservation it wrote.
/// The caller drops the returned pause to let the held request continue.
pub(super) async fn pause_first_reservation(
    t: &TwoBackends,
) -> Result<(
    harnx_a2a_server::fault_injection::Pause,
    tokio::task::JoinHandle<Result<Value>>,
    FirstMessageReservation,
)> {
    paused_first_message(t, Boundary::FirstReservation, 1).await
}

/// Pause replica A's runner fault hooks at `boundary` (the `visit`th time it
/// is reached), dispatch the first message in the background, wait for the
/// boundary, and read back the reservation it wrote: the general form of
/// [`pause_first_reservation`] for tests that vary the boundary.
pub(super) async fn paused_first_message(
    t: &TwoBackends,
    boundary: Boundary,
    visit: usize,
) -> Result<(
    harnx_a2a_server::fault_injection::Pause,
    tokio::task::JoinHandle<Result<Value>>,
    FirstMessageReservation,
)> {
    let (mut pause, send) = pause_and_send_first_message(t, boundary, visit);
    let saved = reached_reservation(&mut pause, t).await?;
    Ok((pause, send, saved))
}

/// Pause replica A's runner fault hooks at `boundary` (the `visit`th time
/// it is reached) and dispatch the first message in the background. The
/// caller waits on the returned pause.
pub(super) fn pause_and_send_first_message(
    t: &TwoBackends,
    boundary: Boundary,
    visit: usize,
) -> (
    harnx_a2a_server::fault_injection::Pause,
    tokio::task::JoinHandle<Result<Value>>,
) {
    let pause = t.a.backend.runner.fault_hooks().pause(boundary, visit);
    let send = send_first_message(t);
    (pause, send)
}

/// Dispatch replica A's first message in the background, leaving the caller
/// free to pause admission before it completes.
pub(super) fn send_first_message(t: &TwoBackends) -> tokio::task::JoinHandle<Result<Value>> {
    tokio::spawn({
        let app = t.a.app.clone();
        async move { rpc(app, "alice", "SendMessage", first_message()).await }
    })
}

/// Wait for a paused fault boundary, then read back the first-message
/// reservation that reached it.
pub(super) async fn reached_reservation(
    pause: &mut harnx_a2a_server::fault_injection::Pause,
    t: &TwoBackends,
) -> Result<FirstMessageReservation> {
    pause.reached().await;
    reservation(t).await
}

/// Pause replica A at its publication boundary (second hit), let its first
/// synchronous call park there, and return the reservation it wrote.
pub(super) async fn paused_publication_first_call(
    t: &TwoBackends,
) -> Result<(
    harnx_a2a_server::fault_injection::Pause,
    Value,
    FirstMessageReservation,
)> {
    let mut pause =
        t.a.backend
            .runner
            .fault_hooks()
            .pause(Boundary::Publication, 2);
    let first = t.a.rpc("alice", "SendMessage", first_message()).await?;
    let saved = reached_reservation(&mut pause, t).await?;
    Ok((pause, first, saved))
}

/// Replica B's committed context for `storage_key`; `why` names the check
/// when no record has been committed yet.
pub(super) async fn replica_b_context(
    t: &TwoBackends,
    storage_key: &str,
    why: &str,
) -> Result<harnx_a2a_server::store::context::ContextSnapshot> {
    t.b.backend
        .store
        .read_context(storage_key)
        .await?
        .with_context(|| why.to_string())
}
async fn requested(t: &TwoBackends, count: usize) -> Result<()> {
    tokio::time::timeout(DEADLINE, async {
        while t.h.llm.requests.lock().len() < count {
            t.h.llm.requested.notified().await;
        }
    })
    .await?;
    Ok(())
}

#[cfg(unix)]
mod broker;
mod cancel;
mod recovery;
