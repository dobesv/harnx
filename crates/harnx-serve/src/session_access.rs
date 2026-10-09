use anyhow::{bail, ensure, Context, Result};
use harnx_core::access_rules::{AccessRules, CallerView};
use harnx_runtime::config::Config;
use harnx_runtime::nats_session_metadata::{session_properties, SessionMetadataStore};
use http::Method;
use hyper::Request;

use crate::{
    ensure_frontend_nats_owner, is_safe_path_segment, percent_decode, sanitize_nats_cluster_error,
    serve_nats_jetstream, session_actor::ResolvedAgentTarget, session_recency_ordering, Server,
    ERROR_STATUS_MARKER,
};

impl Server {
    pub(crate) async fn guard_session_request<B>(
        &self,
        req: &Request<B>,
        target: &ResolvedAgentTarget,
        access: (&AccessRules, CallerView<'_>),
    ) -> Result<()> {
        let (rules, caller) = access;
        let mut segments = req
            .uri()
            .path()
            .trim_start_matches("/v1/agents/")
            .split('/')
            .filter(|segment| !segment.is_empty());
        segments.next(); // Agent was resolved and checked by the shared guard.
        if segments.next() != Some("sessions") {
            return Ok(());
        }
        let agent_ref = self.display_ref(target);
        let Some(session) = segments.next() else {
            if req.method() == Method::POST && !rules.can_create_session(&agent_ref, caller) {
                bail!("session creation requires prompt scope{ERROR_STATUS_MARKER}403");
            }
            return Ok(());
        };
        let session = percent_decode(session);
        ensure!(is_safe_path_segment(&session), "Not Found");
        let key = harnx_core::session_identity::session_key(Some(target.agent()), &session);
        let owner = self
            .read_session_owner(target.cluster(), &key)
            .await?
            .context("Not Found")?;
        ensure!(
            rules.can_access_session(&agent_ref, caller, owner.as_deref()),
            "Not Found"
        );
        Ok(())
    }

    /// Outer `None` means missing metadata; inner `None` means a legacy owner.
    /// CIDs can use their SessionRef::owner() directly as the storage key.
    pub(crate) async fn read_session_owner(
        &self,
        cluster: &str,
        owner_key: &str,
    ) -> Result<Option<Option<String>>> {
        ensure_frontend_nats_owner(cluster).await?;
        let jetstream = serve_nats_jetstream(&self.config, cluster).await?;
        let store = SessionMetadataStore::ensure(&jetstream, 1).await?;
        let Some(record) = store.get(owner_key).await? else {
            return Ok(None);
        };
        Ok(Some(
            session_properties(&record.metadata)?
                .text("user_id")
                .map(str::to_string),
        ))
    }
}

pub(crate) async fn list_target_sessions(
    config: &Config,
    target: &ResolvedAgentTarget,
    access: Option<(&AccessRules, CallerView<'_>)>,
) -> Result<Vec<harnx_runtime::config::SessionMeta>> {
    ensure_frontend_nats_owner(target.cluster()).await?;
    let mut sessions: Vec<_> = config
        .list_remote_sessions_with_meta(target.cluster())
        .await
        .map_err(|error| sanitize_nats_cluster_error(target.cluster(), error))?
        .into_iter()
        // Per-agent endpoints must not leak sessions without agent attribution or for other agents.
        // Missing/empty agent_name stays excluded from per-agent lists until a later backfill pass.
        .filter(|session| session.agent_name.as_deref() == Some(target.agent()))
        .collect();

    if let Some((rules, caller)) = access {
        let agent_ref =
            target.display_ref_with_default_cluster(config.default_cluster_for_display());
        sessions.retain(|session| {
            rules.can_access_session(&agent_ref, caller, session.user_id.as_deref())
        });
    }
    sessions.sort_by(session_recency_ordering);
    Ok(sessions)
}
