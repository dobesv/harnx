//! Rows h–i of the deferred claim: leader reread vs. reservation, and
//! authorization against the record the broker actually returns.
//!
//! [`Server::reserve_and_authorize_claim`] runs after the classification
//! and preflight rows in `session_claim.rs`. A live actor registered in this
//! serve process forces a leader reread (row h); otherwise the ID is
//! reserved through `create_or_read_session_metadata` and the returned
//! record is authorized (row i). The record decides; the guard's earlier
//! missing read never authorizes anything (plan decision D3), and every
//! refusal is the guard's 404 with nothing else done — never the actor's
//! 409 Rejected.

use anyhow::{anyhow, Result};
use harnx_core::access_rules::AccessRules;
use harnx_runtime::nats_session::{create_or_read_session_metadata, SessionMetadataClaim};
use harnx_runtime::nats_session_metadata::{
    session_properties, MetadataRecord, SessionInitializer, SessionMetadataStore,
};

use crate::session_actor::{ResolvedAgentTarget, SessionKey};
use crate::session_claim::guard_not_found;
use crate::{ensure_frontend_nats_owner, serve_nats_jetstream, Server, ERROR_STATUS_MARKER};

/// A reservation failure is broker-side, never an authorization verdict:
/// log the cause server-side and answer with the fixed 503-class body the
/// attachment preflight already uses for the same outage (decision D5).
pub(crate) fn session_metadata_unavailable() -> anyhow::Error {
    anyhow!("session metadata unavailable{ERROR_STATUS_MARKER}503")
}

/// Map a claim-path broker error like the guard maps its own reads (review
/// C5): a typed broker outage answers with the fixed 503-class refusal, so
/// an outage never looks like a denial or a missing ID; every other failure
/// gets the guard's error-status mapping, the same result the guard gives
/// it, without an extra signal that the ID existed.
fn claim_read_error(error: anyhow::Error) -> anyhow::Error {
    if crate::nats_access::is_nats_unavailable(&error) {
        return session_metadata_unavailable();
    }
    crate::guard_error_status(error)
}

// qual:allow(coupling, oi) reason: "The reservation path extends Server by concern, the way session_claim.rs and session_access.rs already do."
impl Server {
    /// Rows h–i: authorize the record the leader has when this serve
    /// process holds a live actor for the key, otherwise reserve the ID
    /// and authorize the record the broker returned. The record decides;
    /// the guard's earlier missing read never authorizes anything (D3).
    /// Denial is the guard's 404 with nothing else done, never the actor's
    /// 409 Rejected.
    pub(crate) async fn reserve_and_authorize_claim(
        &self,
        target: &ResolvedAgentTarget,
        identity: &crate::request_identity::RequestIdentity,
        session: &str,
        scoped: &harnx_runtime::config::GlobalConfig,
    ) -> Result<()> {
        let key = SessionKey::new(target.clone(), session);
        // Row h: a live actor registered in this serve process without
        // metadata means the ID is not claimable (R10); a record found on
        // the leader reread is authorized like any existing one.
        if self.session_registry.has_session(&key) {
            return self
                .authorize_registered_claim(target, &key, identity)
                .await;
        }
        // Row i: reserve, then authorize the returned record.
        self.reserve_and_authorize_record(target, identity, session, scoped)
            .await
    }

    /// Row h, the registered-actor branch: reread the canonical record from
    /// the leader and let it decide with the same fail-closed owner decoder
    /// as row i (review C4/C8). Absent means not claimable (R10); undecodable
    /// properties or a non-string `user_id` cannot establish ownership and
    /// hide behind the guard's 404, admins included, instead of surfacing
    /// the decode error the guard's owner read produces.
    async fn authorize_registered_claim(
        &self,
        target: &ResolvedAgentTarget,
        key: &SessionKey,
        identity: &crate::request_identity::RequestIdentity,
    ) -> Result<()> {
        let record = self
            .read_session_record(target.cluster(), &key.storage_key())
            .await
            .map_err(|error| {
                log::warn!(
                    "session claim reread of '{}' failed: {}",
                    key.storage_key(),
                    crate::nats_access::sanitized_error_chain(&error)
                );
                claim_read_error(error)
            })?;
        let Some(record) = record else {
            return Err(guard_not_found());
        };
        authorize_claim_record(
            self.claim_rules()?,
            &self.display_ref(target),
            identity.caller().view(),
            &record,
        )
    }

    /// Row i, the reservation branch: reserve the ID, then authorize the
    /// record the broker returned — for both a fresh create and a racing
    /// winner. The record decides; a denial is the guard's 404 with
    /// nothing else done (R3, D3).
    async fn reserve_and_authorize_record(
        &self,
        target: &ResolvedAgentTarget,
        identity: &crate::request_identity::RequestIdentity,
        session: &str,
        scoped: &harnx_runtime::config::GlobalConfig,
    ) -> Result<()> {
        let claim = self
            .reserve_session_metadata(target, identity, session, scoped)
            .await?;
        // A transcript stream without metadata (unfinished deletion or an
        // orphan), a vanished create collision, and a generated-ID fallback
        // are not claimable (R10, design step 1).
        let Some(record) = claimed_record(claim, session) else {
            return Err(guard_not_found());
        };
        authorize_claim_record(
            self.claim_rules()?,
            &self.display_ref(target),
            identity.caller().view(),
            &record,
        )
    }

    /// Reserve the canonical metadata record for the caller, or read the
    /// racing winner's record. The initializer stamps the same trusted
    /// request identity the actor's admission stamps. Only a typed broker
    /// outage answers with the fixed 503-class refusal (D5); config,
    /// deserialize and validation failures get the guard's own mapping,
    /// the same result the guard gives them, and the refusal never
    /// reveals whether the ID was claimable (review C5).
    async fn reserve_session_metadata(
        &self,
        target: &ResolvedAgentTarget,
        identity: &crate::request_identity::RequestIdentity,
        session: &str,
        scoped: &harnx_runtime::config::GlobalConfig,
    ) -> Result<SessionMetadataClaim> {
        let reservation = async {
            ensure_frontend_nats_owner(target.cluster()).await?;
            let jetstream = serve_nats_jetstream(&self.config, target.cluster()).await?;
            // The bucket already exists (the guard read it); reconcile its
            // replica count with the cluster's resolved setting instead of
            // pinning 1 (review C13).
            let replicas = self.config.resolve_nats_replicas(target.cluster()).await?;
            let store = SessionMetadataStore::ensure(&jetstream, replicas).await?;
            let initializer = claim_initializer(target, identity, scoped);
            create_or_read_session_metadata(&store, session, &initializer).await
        }
        .await;
        reservation.map_err(|error| {
            log::warn!(
                "session claim for '{session}' could not reserve metadata: {}",
                crate::nats_access::sanitized_error_chain(&error)
            );
            claim_read_error(error)
        })
    }

    /// The rules the server was configured with. Only reached from the
    /// deferred claim path, which the guard defers under rules only; any
    /// other state fails closed with the guard's 404 rather than panicking
    /// (review C11).
    fn claim_rules(&self) -> Result<&AccessRules> {
        self.access_rules.as_deref().ok_or_else(|| {
            log::warn!("deferred session claim reached reservation without access rules");
            guard_not_found()
        })
    }
}

/// The record the reservation returned, or `None` for an outcome that is
/// not claimable: a transcript stream without metadata (unfinished deletion
/// or an orphan), a vanished create collision (R10), or a record whose ID
/// is not the exact requested one — this path never accepts a
/// generated-ID fallback (design step 1).
fn claimed_record(claim: SessionMetadataClaim, session: &str) -> Option<MetadataRecord> {
    let record = match claim {
        SessionMetadataClaim::Created(record) | SessionMetadataClaim::Existing(record) => record,
        SessionMetadataClaim::Orphaned | SessionMetadataClaim::Vanished => return None,
    };
    (record.metadata.session_id == session).then_some(record)
}

/// Authorization against the record the leader actually returned — never
/// the guard's stale missing read (D3). Denial is the guard's 404, with
/// nothing else done (R3). Missing properties remain a legacy owner; an
/// undecodable extension cannot establish ownership and fails closed.
fn authorize_claim_record(
    rules: &AccessRules,
    agent_ref: &str,
    caller: harnx_core::access_rules::CallerView<'_>,
    record: &MetadataRecord,
) -> Result<()> {
    let owner = claim_record_owner(record)?;
    rules
        .can_access_session(agent_ref, caller, owner.as_deref())
        .then_some(())
        .ok_or_else(guard_not_found)
}

/// The record's owner stamp for claim authorization (review C4): present
/// properties name their `user_id`; absent properties stay a legacy owner;
/// undecodable properties, or a present but non-string `user_id`, cannot
/// establish ownership and fail closed with the guard's 404 — admins
/// included, like an inaccessible owner.
fn claim_record_owner(record: &MetadataRecord) -> Result<Option<String>> {
    let properties = session_properties(&record.metadata).map_err(undecodable_claim_record)?;
    match properties.get("user_id") {
        Some(property) if !property.value.is_string() => {
            log::warn!(
                "session record for '{}' has a non-string user_id; failing closed",
                record.metadata.session_id
            );
            Err(guard_not_found())
        }
        _ => Ok(properties.text("user_id").map(str::to_string)),
    }
}

/// A record whose properties cannot be decoded is refused like a missing
/// one, after logging the cause server-side.
fn undecodable_claim_record(error: anyhow::Error) -> anyhow::Error {
    log::warn!(
        "undecodable session record during claim authorization; failing closed: {}",
        crate::nats_access::sanitized_error_chain(&error)
    );
    guard_not_found()
}

/// The initializer reservation stamps: the shared serve-side identity
/// precedence (review C13), then the config default for the cluster —
/// the same default `NatsSession::from_global_config` applies at actor
/// admission, so the claim's owner stamp and the actor's cannot drift.
fn claim_initializer(
    target: &ResolvedAgentTarget,
    identity: &crate::request_identity::RequestIdentity,
    scoped: &harnx_runtime::config::GlobalConfig,
) -> SessionInitializer {
    let snapshot = scoped.read().clone();
    crate::session_actor::identity_stamped_initializer(
        target.agent(),
        &snapshot,
        identity.resolved_user_id(),
        identity.stampable_principals().as_ref(),
    )
    .with_default_user_id(snapshot.default_user_id_for_cluster(target.cluster()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn undecodable_claim_properties_deny_admin_but_absent_properties_remain_legacy() -> Result<()> {
        let rules = AccessRules::from_yaml(
            r#"rules:
  - agents: ['*']
    users: [alice]
    scopes: [admin]
"#,
        )?;
        let caller = harnx_core::access_rules::CallerView::from_users(&["alice"]);
        let mut record = MetadataRecord {
            metadata: harnx_runtime::nats_session_metadata::SessionMetadata::new(
                "claimtest",
                SessionInitializer::named("plain", Default::default()),
            ),
            revision: 1,
        };

        assert!(authorize_claim_record(&rules, "plain", caller, &record).is_ok());

        record.metadata.extensions.insert(
            harnx_runtime::nats_session_metadata::SESSION_PROPERTIES_NAMESPACE.to_string(),
            Value::String("not session properties".to_string()),
        );
        assert_eq!(
            authorize_claim_record(&rules, "plain", caller, &record)
                .expect_err("undecodable properties must deny admin callers")
                .to_string(),
            "Not Found"
        );

        // A present but non-string `user_id` cannot establish ownership
        // either (review C4): fail closed, never a legacy owner.
        record.metadata.extensions.insert(
            harnx_runtime::nats_session_metadata::SESSION_PROPERTIES_NAMESPACE.to_string(),
            serde_json::json!({ "user_id": { "value": 42 } }),
        );
        assert_eq!(
            authorize_claim_record(&rules, "plain", caller, &record)
                .expect_err("a non-string user_id must deny admin callers")
                .to_string(),
            "Not Found"
        );
        Ok(())
    }

    /// Review D1: a typed post-connect write outage maps to the fixed
    /// 503-class refusal, and a non-outage failure keeps the guard's own
    /// mapping — never a blanket 503.
    #[test]
    fn claim_read_error_classifies_outages_and_keeps_other_failures() {
        use async_nats::jetstream::kv::{
            CreateError, CreateErrorKind, UpdateError, UpdateErrorKind,
        };

        // The `kv::Store::create` chain a broker stalled after connect
        // produces: ack timeout, re-labeled Publish with the source kept.
        let stalled = CreateError::with_source(
            CreateErrorKind::Publish,
            UpdateError::new(UpdateErrorKind::TimedOut),
        );
        let mapped = claim_read_error(anyhow::Error::new(stalled));
        assert_eq!(
            mapped.to_string(),
            format!("session metadata unavailable{ERROR_STATUS_MARKER}503")
        );

        // A broker answer — the key already exists — is not an outage: the
        // error keeps its own text and status mapping, with no 503.
        let answer = CreateError::new(CreateErrorKind::AlreadyExists);
        let mapped = claim_read_error(anyhow::Error::new(answer));
        assert!(
            !mapped.to_string().contains("503"),
            "a non-outage failure must not gain a 503 status: {mapped}"
        );
        assert!(
            !crate::nats_access::is_nats_unavailable(&mapped),
            "a non-outage failure must stay classifiable as an answer"
        );
    }
}
