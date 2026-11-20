//! First-message recovery stamps the winning caller's principals, not the
//! retrying or recovering request's.

use super::admission::remove_owner_lease;
use super::supervision::{pause_first_reservation, supervise, terminal};
use super::*;
use a2a_lf::TaskState;
use harnx_a2a_server::{
    handler::{Backend, BackendConfig, HarnxHandler},
    identity::Identity,
    input_map::InputLimits,
    routes,
    runner::Runner,
    store::A2aStore,
};
use harnx_core::{
    user_aliases::UserAliases,
    user_principals::{from_resolved, UserPrincipals},
};
use harnx_runtime::SessionActivationRoute;
use std::sync::Arc;

const ALIASES: &str =
    "- name: Team\n  identities: [alice, bob]\n  groups: [engineering]\n  roles: [reviewer]\n";

fn expected_principals() -> UserPrincipals {
    from_resolved(
        Some("alice"),
        Some(&["alice".into(), "bob".into()]),
        &["engineering".into()],
        &["reviewer".into()],
    )
}

/// A replica whose HTTP identity expands aliases, so the winning first
/// message resolves real memberships.
fn principled_replica(h: &Harness, store: Arc<A2aStore>, runner: Arc<Runner>) -> Result<Replica> {
    let backend = Arc::new(Backend::new(
        runner,
        store,
        BackendConfig {
            config: h.config.clone(),
            route: SessionActivationRoute::ClusterShared,
            abort: harnx_core::abort::create_abort_signal(),
        },
    ));
    let identity = Identity::new(&["X-User-ID".into()])?
        .with_user_aliases(Some(Arc::new(UserAliases::from_yaml(ALIASES)?)));
    let mut export = h.export.clone();
    export.lookup_keys = vec![export.public_name.clone()];
    let app =
        routes::router_with_access_rules(&[export], None, identity, None, |export, identity| {
            Arc::new(HarnxHandler::new(
                export.clone(),
                identity,
                backend.clone(),
                InputLimits::default(),
            ))
        })?;
    Ok(Replica { backend, app })
}

fn principled_backend(h: &Harness) -> Result<Replica> {
    // One store shared by runner and backend, like TwoBackends::start_script.
    let store = Arc::new(A2aStore::new(h.metadata.clone()));
    principled_replica(h, store.clone(), Runner::new(store))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_recovery_stamps_the_winning_callers_principals() -> Result<()> {
    let h = Harness::start(Script::Text).await?;
    let t = TwoBackends {
        a: principled_backend(&h)?,
        b: principled_backend(&h)?,
        h,
    };

    // Interrupt the winner right after its reservation is written: metadata
    // does not exist yet.
    let (pause, send, saved) = pause_first_reservation(&t).await?;
    assert_eq!(
        saved.principals.as_ref().context("reservation snapshot")?,
        &expected_principals(),
        "the winning caller's snapshot travels with the reservation"
    );
    let metadata_key =
        harnx_core::session_identity::session_key(Some("runner"), &saved.allocation.local_id);
    assert!(
        t.h.metadata.get(&metadata_key).await?.is_none(),
        "recovery, not the interrupted request, must create the metadata"
    );

    // The interrupted caller never returns; background recovery takes over.
    remove_owner_lease(&t, &saved.allocation.storage_key).await?;
    let restarted = principled_backend(&t.h)?;
    supervise(&t, &restarted);
    assert_eq!(
        terminal(&t, &saved.allocation.task_id)
            .await?
            .task
            .status
            .state,
        TaskState::Failed
    );
    assert!(t.h.llm.requests.lock().is_empty());
    let record =
        t.h.metadata
            .get(&metadata_key)
            .await?
            .context("recovered metadata")?;
    assert_eq!(
        harnx_runtime::nats_session_metadata::user_principals(&record.metadata),
        expected_principals(),
        "recovery stamps the original caller, not the recovering server"
    );

    drop(pause);
    let _ = send.await?;
    restarted.backend.abort.set_ctrlc();
    t.a.backend.runner.shutdown().await;
    Ok(())
}
