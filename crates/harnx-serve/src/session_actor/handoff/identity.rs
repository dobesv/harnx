//! The identity a handoff target inherits from its source session.

use super::SessionActor;
use super::SessionPromptOptions;
use anyhow::Context;

/// Identity a handoff target inherits from its source session, stamped read-only
/// when the target session's metadata is created.
#[derive(Debug, Default)]
pub(super) struct InheritedIdentity {
    pub(super) user_id: Option<String>,
    pub(super) user_principals: Option<harnx_core::user_principals::UserPrincipals>,
}

impl InheritedIdentity {
    pub(super) fn into_prompt_options(
        self,
        parent: harnx_runtime::nats_session_metadata::RunLimitsRecord,
        web_base_url: Option<&str>,
    ) -> SessionPromptOptions {
        SessionPromptOptions {
            runtime_parent: Some(Box::new(parent)),
            web_base_url: web_base_url.map(str::to_string),
            user_id: self.user_id,
            user_principals: self.user_principals,
            ..Default::default()
        }
    }
}

impl SessionActor {
    /// The identity a handoff target inherits: the source session's owner id
    /// and caller-principals snapshot, copied verbatim rather than recomputed.
    pub(super) async fn handoff_identity(&self) -> anyhow::Result<InheritedIdentity> {
        let config = self.prompt_config().await.read().clone();
        lookup_handoff_identity(&config, &self.key).await
    }

    pub(super) fn handoff_identity_error(&self) -> anyhow::Error {
        source_identity_unavailable(self.key.session())
    }
}

pub(crate) async fn lookup_handoff_identity(
    config: &harnx_runtime::config::Config,
    key: &super::SessionKey,
) -> anyhow::Result<InheritedIdentity> {
    let jetstream = crate::serve_nats_jetstream(config, key.cluster()).await?;
    let store =
        harnx_runtime::nats_session_metadata::SessionMetadataStore::ensure(&jetstream, 1).await?;
    let record = store
        .get_for_agent(key.session(), key.agent())
        .await?
        .map(|record| record.metadata);
    decode_handoff_identity(key.session(), Ok(record))
}

fn decode_handoff_identity(
    session_id: &str,
    record: anyhow::Result<Option<harnx_runtime::nats_session_metadata::SessionMetadata>>,
) -> anyhow::Result<InheritedIdentity> {
    let Some(metadata) = record? else {
        log::warn!(
            "handoff source metadata is missing; using anonymous identity: session_id={session_id} category=source_session_metadata_missing"
        );
        return Ok(InheritedIdentity::default());
    };
    let inherited = harnx_runtime::nats_session_metadata::session_properties(&metadata)
        .context("decode source session properties for handoff")?
        .inherited_identity();
    Ok(InheritedIdentity {
        user_id: inherited.text("user_id").map(str::to_string),
        user_principals: Some(inherited.user_principals())
            .filter(|principals| !principals.is_anonymous()),
    })
}

pub(super) fn source_identity_unavailable(session_id: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "handoff identity unavailable (category=source_identity_unavailable) for source session '{session_id}'"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::user_principals::from_resolved;
    use harnx_runtime::nats_session_metadata::SessionInitializer;
    use serde_json::json;

    fn session_metadata(
        properties: serde_json::Value,
    ) -> harnx_runtime::nats_session_metadata::SessionMetadata {
        let mut metadata = harnx_runtime::nats_session_metadata::SessionMetadata::new(
            "source",
            SessionInitializer::named("source-agent", Default::default()),
        );
        metadata.extensions.insert(
            harnx_runtime::nats_session_metadata::SESSION_PROPERTIES_NAMESPACE.into(),
            properties,
        );
        metadata
    }

    #[test]
    fn missing_source_record_yields_anonymous_identity() {
        let identity = decode_handoff_identity("source", Ok(None)).unwrap();
        assert!(identity.user_id.is_none());
        assert!(identity.user_principals.is_none());
    }

    #[test]
    fn malformed_source_properties_fail_handoff_identity_lookup() {
        let error = decode_handoff_identity("source", Ok(Some(session_metadata(json!("broken")))))
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("decode source session properties"));
    }

    #[test]
    fn store_errors_fail_handoff_identity_lookup() {
        let error =
            decode_handoff_identity("source", Err(anyhow::anyhow!("metadata store unavailable")))
                .unwrap_err();
        assert!(error.to_string().contains("metadata store unavailable"));
    }

    #[test]
    fn identity_lookup_failure_reports_only_category_and_session_id() {
        assert_eq!(
            source_identity_unavailable("source").to_string(),
            "handoff identity unavailable (category=source_identity_unavailable) for source session 'source'"
        );
    }
    #[test]
    fn identity_is_carried_as_prompt_options_for_self_handoff() {
        let principals = from_resolved(
            Some("alice"),
            Some(&["alice".into(), "alice@example.test".into()]),
            &["engineering".into()],
            &["reviewer".into()],
        );
        let identity = InheritedIdentity {
            user_id: Some("alice".into()),
            user_principals: Some(principals.clone()),
        };
        let parent = serde_json::from_value(json!({
            "run_id": "test-run",
            "invocation_id": "test-invocation",
            "admitted_at": "2026-01-01T00:00:00Z",
            "deadline": null,
            "policy_source": "global_default",
            "parent_invocation": null
        }))
        .unwrap();

        let options = identity.into_prompt_options(parent, None);
        assert_eq!(options.user_id.as_deref(), Some("alice"));
        assert_eq!(options.user_principals, Some(principals));
    }
}
