//! Recording where a session opens in the Web UI.

use super::*;
use harnx_runtime::nats_session_metadata::PropertySource;

/// How long prompt admission waits for the address to be recorded. The
/// address is a convenience, so a slow metadata bucket must not hold up the
/// prompt, or the commands queued behind it on this actor.
const RECORD_TIMEOUT: Duration = Duration::from_secs(2);

/// What a session actor knows about its session's Web UI address.
#[derive(Default)]
pub(super) struct WebAddress {
    /// The configured public URL, which wins over anything inferred.
    configured_base: Option<String>,
    /// The base inferred from the latest prompt's request.
    inferred_base: Option<String>,
    /// Set once this actor has recorded the address or found it in place.
    recorded: bool,
}

impl WebAddress {
    pub(super) fn new(config: &Config) -> Self {
        Self {
            configured_base: crate::web_url::public_url_or_warn(config),
            ..Self::default()
        }
    }

    pub(super) fn inferred_base(&self) -> Option<&str> {
        self.inferred_base.as_deref()
    }

    fn base(&self) -> Option<(&str, PropertySource)> {
        match (&self.configured_base, &self.inferred_base) {
            (Some(base), _) => Some((base, PropertySource::Configured)),
            (None, Some(base)) => Some((base, PropertySource::Inferred)),
            (None, None) => None,
        }
    }
}

impl SessionActor {
    /// Record the session's Web UI address. Agents read it as a convenience,
    /// so a failure here is logged and never stops the prompt.
    pub(super) async fn record_web_session_url(
        &mut self,
        session: &NatsSession,
        options: &SessionPromptOptions,
    ) {
        if let Some(base) = &options.web_base_url {
            self.web_address.inferred_base = Some(base.clone());
        }
        if self.web_address.recorded {
            return;
        }
        let Some((base, source)) = self.web_address.base() else {
            return;
        };
        let url = crate::web_url::session_url(
            base,
            &self.key,
            self.actor_config.base_config.default_cluster_for_display(),
        );
        let record =
            session
                .metadata_store()
                .record_web_session_url(session.storage_key(), &url, source);
        let error = match tokio::time::timeout(RECORD_TIMEOUT, record).await {
            Ok(Ok(_)) => {
                self.web_address.recorded = true;
                return;
            }
            Ok(Err(error)) => format!("{error:#}"),
            Err(_) => format!("timed out after {RECORD_TIMEOUT:?}"),
        };
        log::warn!(
            "could not record the session's Web UI address: agent={} session_id={} error={error}",
            self.key.display_ref(),
            self.key.session
        );
    }
}
