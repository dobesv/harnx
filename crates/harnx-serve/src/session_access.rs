use anyhow::{bail, ensure, Context, Result};
use harnx_core::access_rules::AccessRules;
use harnx_runtime::nats_session_metadata::{session_properties, SessionMetadataStore};
use http::Method;
use hyper::Request;

use crate::{
    ensure_frontend_nats_owner, is_safe_path_segment, percent_decode, serve_nats_jetstream,
    session_actor::ResolvedAgentTarget, Server, ERROR_STATUS_MARKER,
};

impl Server {
    pub(crate) async fn guard_session_request<B>(
        &self,
        req: &Request<B>,
        target: &ResolvedAgentTarget,
        access: (&AccessRules, &[&str]),
    ) -> Result<()> {
        let (rules, identities) = access;
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
            if req.method() == Method::POST && !rules.can_create_session(&agent_ref, identities) {
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
            rules.can_access_session(&agent_ref, identities, owner.as_deref()),
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
