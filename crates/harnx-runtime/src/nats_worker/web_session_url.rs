//! Best-effort Web UI addresses for sessions executed by a worker.
//!
//! Workers record `web_session_url` for named-agent sessions that don't yet have
//! one (handoff targets, sub-agents, A2A tasks). The hook runs once per
//! `execute_session`, before turn execution. URL construction uses the session's
//! bare route ID (`metadata.session_id`); storage mutations use the hashed
//! activation key (`activation.session_id`). See
//! `harnx_core::session_identity::session_key` for the derivation.

use super::daemon_runtime::WorkerRuntime;
use crate::config::{nats_routing_from_env, NatsRouting};
use crate::nats_session_metadata::{session_properties, SessionMetadata, WEB_SESSION_URL_PROPERTY};
use crate::web_url::{agent_display_ref, normalize_public_url, session_url};
use std::time::Duration;

// A convenience link must not hold up the session when metadata storage is slow.
const RECORD_TIMEOUT: Duration = Duration::from_secs(2);

/// Bundles arguments for URL construction to stay within CodeScene's argument limit.
struct UrlComponents<'a> {
    base: &'a str,
    name: &'a str,
    cluster: &'a str,
    default_cluster: Option<&'a str>,
    session_id: &'a str,
}

impl WorkerRuntime {
    pub(super) async fn record_web_session_url(
        &self,
        storage_key: &str,
        metadata: &SessionMetadata,
    ) {
        let configured_base = self.config.read().serve_public_url.clone();
        let base = match normalize_public_url(configured_base.as_deref()) {
            Ok(Some(base)) => base,
            Ok(None) => return,
            Err(error) => {
                log::warn!("ignoring worker serve_public_url: {error:#}");
                return;
            }
        };
        let Some(name) = metadata.agent.name() else {
            return;
        };
        match session_properties(metadata) {
            Ok(properties) if properties.get(WEB_SESSION_URL_PROPERTY).is_some() => return,
            Ok(_) => {}
            Err(error) => {
                log::warn!(
                    "could not read the session's Web UI address: agent={name} session_id={} error={error:#}",
                    metadata.session_id
                );
                return;
            }
        }
        // Read the frontend's display default without applying frontend routing.
        // Workers must NOT call `apply_frontend_nats_routing()`.
        let default_cluster = match nats_routing_from_env() {
            NatsRouting::Cluster(name) => Some(name),
            _ => None,
        };
        let url = worker_session_url(UrlComponents {
            base: &base,
            name,
            cluster: &self.cluster,
            default_cluster: default_cluster.as_deref(),
            session_id: &metadata.session_id,
        });
        // The route uses the bare ID above; metadata writes use the activation's hash.
        let record = self
            .session_metadata
            .record_web_session_url_if_absent(storage_key, &url);
        let error = match tokio::time::timeout(RECORD_TIMEOUT, record).await {
            Ok(Ok(_)) => return,
            Ok(Err(error)) => format!("{error:#}"),
            Err(_) => format!("timed out after {RECORD_TIMEOUT:?}"),
        };
        log::warn!(
            "could not record the session's Web UI address: agent={name} session_id={} error={error}",
            metadata.session_id
        );
    }
}

fn worker_session_url(components: UrlComponents<'_>) -> String {
    session_url(
        components.base,
        &agent_display_ref(
            components.name,
            components.cluster,
            components.default_cluster,
        ),
        components.session_id,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LOCAL_CLUSTER_KEY;

    #[test]
    fn worker_urls_use_the_frontend_display_default() {
        for (cluster, default, agent) in [
            (LOCAL_CLUSTER_KEY, None, "metis"),
            (LOCAL_CLUSTER_KEY, Some("prod"), "metis"),
            ("prod", Some("prod"), "metis"),
            ("prod", None, "metis%40prod"),
            ("prod", Some("other"), "metis%40prod"),
        ] {
            assert_eq!(
                worker_session_url(UrlComponents {
                    base: "https://example.test",
                    name: "metis",
                    cluster,
                    default_cluster: default,
                    session_id: "bare-id",
                }),
                format!("https://example.test/agents/{agent}/sessions/bare-id")
            );
        }
    }

    #[test]
    fn worker_urls_encode_agent_and_bare_session_id_under_a_prefix() {
        assert_eq!(
            worker_session_url(UrlComponents {
                base: "https://example.test/harnx",
                name: "pantheon/atlas",
                cluster: "prod",
                default_cluster: None,
                session_id: "session /雪",
            }),
            "https://example.test/harnx/agents/pantheon%2Fatlas%40prod/sessions/session%20%2F%E9%9B%AA"
        );
    }
}
