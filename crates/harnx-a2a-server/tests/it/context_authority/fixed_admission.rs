//! Task3 integration proof only; production runner/HTTP admission stays unchanged.
use super::*;
use harnx_core::{message::MessageContent, session::SessionLogEntry};
use harnx_runtime::{
    nats_session::{
        fixed_admission::{FixedAdmissionOutcome, FixedAdmissionTicket},
        fixed_admission_faults::FixedAdmissionFaults,
    },
    nats_session_log::NatsSessionLog,
    NatsSession, NatsSessionConfig, SessionActivationRoute, SessionInitializer,
};

#[tokio::test]
async fn same_document_ticket_survives_takeover_and_closes_paused_old_runtime_append() -> Result<()>
{
    let f = Fixture::start().await?;
    let session = NatsSession::new(
        NatsSessionConfig {
            cluster: "local".into(),
            session_id: Some(LOCAL.into()),
            initializer: SessionInitializer::inline("", Default::default(), Default::default()),
            activation_route: SessionActivationRoute::ClusterShared,
        },
        f.js.client(),
        f.js.clone(),
        harnx_core::abort::create_abort_signal(),
    )
    .await?
    .with_external_admission();
    let storage = session.storage_key();
    let mut params = f.params("boot-a");
    params.session_id = storage;
    let a = NatsSessionLease::acquire_scoped(params, "a2a")
        .await?
        .context("A lease")?;
    let claim =
        f.a.prepare_context_claim(
            ContextIdentity {
                storage_key: storage,
                local_id: LOCAL,
            },
            &a,
            "claim-a",
        )
        .await?;
    let claimed = f.a.commit_context(&claim).await?;
    let ticket = session
        .prepare_fixed_admission(
            "runtime-invocation".into(),
            "runtime-prompt".into(),
            "runtime-close".into(),
        )
        .await?;
    let mut active = active_task();
    active.admission = AdmissionState {
        invocation_id: ticket.invocation_id().into(),
        prompt_id: ticket.prompt_id().into(),
        closure_id: Some(ticket.closure_id().into()),
        fixed_predecessor: ticket.expected_predecessor(),
        phase: AdmissionPhase::Reserved,
        prompt_sequence: None,
    };
    let write =
        f.a.prepare_context_update(
            storage,
            &claimed.version()?,
            "reserve-fixed-ticket",
            |state| state.active = Some(active),
        )
        .await?;
    let reserved = f.a.commit_context(&write).await?;
    assert!(f
        .a
        .prepare_context_update(storage, &reserved.version()?, "replace-close-id", |state| {
            state.active.as_mut().unwrap().admission.closure_id = Some("replacement-close".into());
        })
        .await
        .is_err());
    let stale =
        f.a.prepare_context_update(storage, &reserved.version()?, "stale-admission", |_| {})
            .await?;
    let faults = FixedAdmissionFaults::new();
    let old = session.clone().with_fixed_admission_faults(faults.clone());
    let pause = faults.pause_next();
    let delayed = tokio::spawn({
        let ticket = ticket.clone();
        async move {
            old.append_fixed_prompt(&ticket, MessageContent::Text("never execute".into()))
                .await
        }
    });
    pause.wait_reached().await?;
    expiry(&f, &a).await?;
    let mut params = f.params("boot-b");
    params.session_id = storage;
    let b = NatsSessionLease::acquire_scoped(params, "a2a")
        .await?
        .context("B lease")?;
    let claim =
        f.b.prepare_context_claim(
            ContextIdentity {
                storage_key: storage,
                local_id: LOCAL,
            },
            &b,
            "takeover-b",
        )
        .await?;
    let successor = f.b.commit_context(&claim).await?;
    assert_eq!(successor.document.epoch, 2);
    assert!(f.a.commit_context(&stale).await.is_err());
    let admission = &successor
        .document
        .state
        .active
        .as_ref()
        .context("retained task")?
        .admission;
    let restored = FixedAdmissionTicket::from_parts(
        storage.into(),
        admission.invocation_id.clone(),
        admission.prompt_id.clone(),
        admission
            .closure_id
            .clone()
            .context("persisted closure identity")?,
        admission.fixed_predecessor,
    )?;
    assert_eq!(ticket, restored);
    let closed = session.close_fixed_admission(&restored).await?;
    assert_eq!(
        closed,
        FixedAdmissionOutcome::Closed {
            closure_sequence: 1
        }
    );
    let write =
        f.b.prepare_context_update(
            storage,
            &successor.version()?,
            "confirm-runtime-close",
            |state| {
                let active = state.active.as_mut().unwrap();
                active.admission.phase = AdmissionPhase::Closed;
                active.stop_confirmed = true;
            },
        )
        .await?;
    f.b.commit_context(&write).await?;
    drop(pause);
    assert_eq!(delayed.await??, closed);
    let log = NatsSessionLog::new(f.js.clone(), storage);
    let entries = log.load_events_latest_async().await?;
    assert_eq!(entries.len(), 1);
    assert!(matches!(
        entries[0].1,
        SessionLogEntry::AdmissionClosed { .. }
    ));
    assert_eq!(
        harnx_core::session_reconstruct::pending_prompt_seq(&entries),
        None
    );
    assert_eq!(session.activate_pending_turn().await?, None);
    assert!(session
        .metadata_store()
        .active_admission(storage, &entries)
        .await?
        .is_none());
    b.release().await?;
    Ok(())
}
