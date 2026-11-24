use anyhow::{bail, ensure, Context, Result};
use harnx_core::access_rules::{AccessRules, CallerView};
use harnx_core::cid_url::CidUrl;
use harnx_runtime::config::Config;
use harnx_runtime::nats_admin::kv_bucket_missing;
use harnx_runtime::nats_session_metadata::{
    session_properties, SessionMetadataStore, SESSION_METADATA_BUCKET,
};
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
        read_session_owner(&self.config, cluster, owner_key).await
    }
}

/// Read a session's owner id from its metadata record in `cluster`.
///
/// Outer `None` means missing metadata; inner `None` means a legacy owner.
pub(crate) async fn read_session_owner(
    config: &Config,
    cluster: &str,
    owner_key: &str,
) -> Result<Option<Option<String>>> {
    let Some(store) = open_session_metadata_readonly(config, cluster).await? else {
        return Ok(None);
    };
    let Some(record) = store.get(owner_key).await? else {
        return Ok(None);
    };
    Ok(Some(
        session_properties(&record.metadata)?
            .text("user_id")
            .map(str::to_string),
    ))
}

/// Open the session metadata bucket in `cluster` read-only, without
/// provisioning it: `get_key_value` instead of an `ensure` that would create
/// or reconcile the bucket on a read path. Reads stay leader-backed —
/// [`SessionMetadataStore::get`] reads through `STREAM.MSG.GET` on the stream
/// leader. `None` means the bucket does not exist yet, so no session
/// metadata exists anywhere in the cluster: every key reads as absent.
pub(crate) async fn open_session_metadata_readonly(
    config: &Config,
    cluster: &str,
) -> Result<Option<SessionMetadataStore>> {
    ensure_frontend_nats_owner(cluster).await?;
    let jetstream = serve_nats_jetstream(config, cluster).await?;
    match jetstream.get_key_value(SESSION_METADATA_BUCKET).await {
        Ok(kv_store) => Ok(Some(SessionMetadataStore::from_store(
            kv_store,
            jetstream.client().clone(),
        ))),
        Err(error) if kv_bucket_missing(&error) => Ok(None),
        Err(error) => Err(anyhow::Error::from(error)).with_context(|| {
            format!("Failed to open session metadata bucket '{SESSION_METADATA_BUCKET}'")
        }),
    }
}

/// What checking one `cid:media` source session against the access rules
/// decided. Splitting the infrastructure failure from the authorization
/// verdict lets callers fail closed without disguising a broker outage as a
/// denial.
pub(crate) enum SourceAccess {
    /// The caller may access the source session.
    Authorized,
    /// The source has no named agent, no metadata record, or the caller may
    /// not access it. Fail closed, with no reason reaching the client.
    Denied,
    /// The source's owner metadata could not be read. Infrastructure
    /// failure, not an authorization verdict; the cause is logged at
    /// `warn` where the read failed.
    Unavailable,
}

/// One prompt's authorization context: the read-only metadata handle its
/// source reads share, the rules, the trusted caller identity, and the
/// cluster the refs will be resolved in — the destination cluster, not
/// necessarily the server's default cluster.
pub(crate) struct SourceCheck<'a> {
    pub(crate) store: &'a SessionMetadataStore,
    pub(crate) rules: &'a AccessRules,
    pub(crate) caller: CallerView<'a>,
    pub(crate) cluster: &'a str,
    pub(crate) default_cluster: Option<&'a str>,
}

impl SourceCheck<'_> {
    /// Authorize the source session named by a cid URL against the access
    /// rules.
    ///
    /// The policy check matches the route guard: the CID agent's normalized
    /// reference for the cluster (bare for the default cluster,
    /// `agent@cluster` otherwise) and `rules.can_access_session`
    /// semantics, so an admin caller or one authorized for the source
    /// session passes while everyone else is denied. Fail closed: an
    /// unattributed source (no agent, no metadata record) denies, an
    /// unreadable one reports [`SourceAccess::Unavailable`], and the
    /// outcome never depends on whether the referenced object exists.
    // qual:allow(iosp) — verdict assembly: one leader-backed metadata read (extracted into source_owner) plus the route guard's own policy check; splitting it would only relocate the composition.
    pub(crate) async fn access(&self, url: &CidUrl) -> SourceAccess {
        let Some(agent) = url.session().agent.as_deref() else {
            return SourceAccess::Denied;
        };
        let owner = match self.source_owner(url).await {
            SourceOwner::Read(owner) => owner,
            SourceOwner::Absent => return SourceAccess::Denied,
            SourceOwner::Unreadable => return SourceAccess::Unavailable,
        };
        let agent_ref =
            harnx_runtime::web_url::agent_display_ref(agent, self.cluster, self.default_cluster);
        if self
            .rules
            .can_access_session(&agent_ref, self.caller, owner.as_deref())
        {
            SourceAccess::Authorized
        } else {
            SourceAccess::Denied
        }
    }

    /// The source session's owner id, `None` for a legacy owner. `Absent` and
    /// `Unreadable` both deny fail closed upstream, without the caller
    /// learning why.
    async fn source_owner(&self, url: &CidUrl) -> SourceOwner {
        let record = match self.store.get(&url.session().owner()).await {
            Ok(Some(record)) => record,
            Ok(None) => return SourceOwner::Absent,
            Err(error) => {
                log::warn!(
                    "CID source metadata read failed for {url} in cluster '{}': {}",
                    self.cluster,
                    crate::nats_access::sanitized_error_chain(&error)
                );
                // Only confirmed broker unavailability is an outage. A read
                // that fails any other way — an undecodable record, a stream
                // that vanished under the handle — is the caller's generic
                // denial, indistinguishable from an absent source (D3).
                return read_failure(&error);
            }
        };
        // A record that cannot be parsed attributes no owner, not even a
        // legacy one: absent, deny for everyone, admin callers included.
        match session_properties(&record.metadata) {
            Ok(properties) => SourceOwner::Read(properties.text("user_id").map(str::to_string)),
            Err(_) => SourceOwner::Absent,
        }
    }
}

/// What the source session's owner metadata read found.
enum SourceOwner {
    Read(Option<String>),
    Absent,
    Unreadable,
}

/// What a failed source metadata read means: only confirmed broker
/// unavailability is an outage; any other failure — an undecodable record,
/// a missing stream — denies like an absent source, so a corrupt record is
/// indistinguishable from a missing source (D3).
fn read_failure(error: &anyhow::Error) -> SourceOwner {
    if crate::nats_access::is_nats_unavailable(error) {
        SourceOwner::Unreadable
    } else {
        SourceOwner::Absent
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A private throwaway KV bucket, so breaking its stream cannot disturb
    /// the other tests sharing the local broker. `SourceCheck` reads whatever
    /// store it is handed and never names the bucket itself.
    async fn throwaway_store(
        jetstream: &async_nats::jetstream::Context,
    ) -> anyhow::Result<SessionMetadataStore> {
        let bucket = format!("cid_source_access_{}", uuid::Uuid::new_v4().simple());
        jetstream
            .create_key_value(async_nats::jetstream::kv::Config {
                bucket: bucket.clone(),
                history: 1,
                num_replicas: 1,
                ..Default::default()
            })
            .await?;
        let kv_store = jetstream.get_key_value(&bucket).await?;
        Ok(SessionMetadataStore::from_store(
            kv_store,
            jetstream.client().clone(),
        ))
    }

    fn check<'a>(store: &'a SessionMetadataStore, rules: &'a AccessRules) -> SourceCheck<'a> {
        SourceCheck {
            store,
            rules,
            caller: CallerView::from_users(&["alice"]),
            cluster: "default",
            default_cluster: Some("default"),
        }
    }

    fn source_ref() -> CidUrl {
        CidUrl::parse(
            "cid:media:allowed/sess01/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .expect("parse cid ref")
    }

    fn rules() -> AccessRules {
        AccessRules::from_yaml(
            "rules:\n  - agents: [allowed]\n    scopes: [prompt]\n    users: [alice]\n",
        )
        .expect("parse access rules")
    }

    /// The fail-closed branch: a source whose metadata read fails denies,
    /// never Authorizes. The check reads whatever store it is handed, so the
    /// read failure is forced by deleting a throwaway bucket's stream under
    /// the store handle. A missing stream is not broker unavailability, so
    /// the denial is the generic one (D3), not the 503-class verdict.
    #[tokio::test]
    async fn cid_source_access_fails_closed_when_metadata_read_fails() {
        harnx_core::require_nextest();
        let sandbox = crate::test_support::TestConfigSandbox::new();
        if !crate::test_support::ensure_test_nats().await {
            return;
        }
        let mut config = sandbox.config();
        config.dry_run = false;
        let jetstream = config
            .nats_jetstream(harnx_runtime::config::LOCAL_CLUSTER_KEY)
            .await
            .expect("local NATS context");

        let store = throwaway_store(&jetstream).await.expect("throwaway store");
        jetstream
            .delete_stream(format!("KV_{}", store.kv_store().name))
            .await
            .expect("delete throwaway KV stream");

        let rules = rules();
        let url = source_ref();
        assert!(
            matches!(
                check(&store, &rules).access(&url).await,
                SourceAccess::Denied
            ),
            "an unreadable source must never be Authorized"
        );
    }

    /// An undecodable source record denies with the same generic 404 as an
    /// absent one (D3): only broker unavailability maps to Unavailable, so
    /// a corrupt record is not distinguishable from a missing source.
    #[tokio::test]
    async fn cid_source_access_denies_undecodable_metadata_like_an_absent_source() {
        harnx_core::require_nextest();
        let sandbox = crate::test_support::TestConfigSandbox::new();
        if !crate::test_support::ensure_test_nats().await {
            return;
        }
        let mut config = sandbox.config();
        config.dry_run = false;
        let jetstream = config
            .nats_jetstream(harnx_runtime::config::LOCAL_CLUSTER_KEY)
            .await
            .expect("local NATS context");

        let store = throwaway_store(&jetstream).await.expect("throwaway store");
        let url = source_ref();
        // The exact metadata key the store reads, holding bytes that cannot
        // decode as a session record.
        store
            .kv_store()
            .put(
                &format!("sessions/{}/meta", url.session().owner()),
                bytes::Bytes::from_static(b"not a session metadata record"),
            )
            .await
            .expect("write corrupt record");

        let rules = rules();
        assert!(
            matches!(
                check(&store, &rules).access(&url).await,
                SourceAccess::Denied
            ),
            "an undecodable record must deny exactly like an absent source"
        );
    }

    /// Only confirmed broker unavailability is an outage (D3): the real
    /// JetStream offline wrapper classifies Unreadable, while an undecodable
    /// record, a missing stream and an ordinary decode failure all deny.
    #[test]
    fn source_read_failure_classifies_only_broker_unavailability_as_outage() {
        fn jetstream_api_error(
            code: async_nats::jetstream::ErrorCode,
        ) -> async_nats::jetstream::Error {
            // `jetstream::Error` has no public constructor; deserialize the
            // real server API error shape, the way the server sends it.
            let json = format!(
                r#"{{"code":503,"err_code":{},"description":"temporarily unavailable"}}"#,
                code.0
            );
            serde_json::from_str(&json).expect("parse jetstream error")
        }

        // A leader read that the JetStream API answered with an offline
        // code, wrapped the way async-nats wraps it.
        let offline = anyhow::Error::new(async_nats::jetstream::stream::LastRawMessageError::new(
            async_nats::jetstream::stream::LastRawMessageErrorKind::JetStream(jetstream_api_error(
                async_nats::jetstream::ErrorCode::CLUSTER_NOT_AVAILABLE,
            )),
        ))
        .context("read key k from bucket b");
        assert!(matches!(read_failure(&offline), SourceOwner::Unreadable));

        // The recovery-deadline marker a hung leader read ends in.
        let deadline = anyhow::Error::new(harnx_nats_common::recovery::RecoveryDeadlineExceeded)
            .context("read key k from bucket b");
        assert!(matches!(read_failure(&deadline), SourceOwner::Unreadable));

        // A stream that vanished under the handle is an absence answer.
        let missing_stream =
            anyhow::Error::new(async_nats::jetstream::context::GetStreamError::new(
                async_nats::jetstream::context::GetStreamErrorKind::JetStream(
                    serde_json::from_str(
                        r#"{"code":400,"err_code":10059,"description":"stream not found"}"#,
                    )
                    .expect("parse jetstream error"),
                ),
            ))
            .context("open the session metadata bucket");
        assert!(matches!(read_failure(&missing_stream), SourceOwner::Absent));

        // An undecodable record is not an outage.
        let undecodable = anyhow::anyhow!("Failed to deserialize session metadata 'k'");
        assert!(matches!(read_failure(&undecodable), SourceOwner::Absent));
    }
}
