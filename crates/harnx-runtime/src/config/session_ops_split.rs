//! Session lifecycle and editing methods extracted from config/mod.rs for code health.
use super::*;
use crate::nats_admin::kv_bucket_missing;
use crate::nats_session_metadata::{ListedSession, SessionMetadataStore, SESSION_METADATA_BUCKET};
use std::time::Duration;

const REMOTE_SESSION_LIST_TIMEOUT: Duration = Duration::from_secs(10);

/// Convert canonical metadata joined with activity to `SessionMeta`.
///
/// This mapping is used by `list_remote_sessions_with_meta` to translate
/// NATS KV records into the picker/CLI-friendly `SessionMeta` format.
fn listed_session_to_meta(record: &ListedSession) -> SessionMeta {
    let modified = record
        .activity
        .as_ref()
        .map(|activity| activity.last_activity_at)
        .unwrap_or(record.metadata.created_at);
    SessionMeta {
        id: record.metadata.session_id.clone(),
        session_id: Some(record.metadata.session_id.clone()),
        agent_name: record.metadata.agent.name().map(str::to_string),
        title: record.metadata.title.value.clone(),
        modified: Some(modified.into()),
        contexts: crate::nats_session_metadata::execution_contexts(&record.metadata)
            .unwrap_or_else(|error| {
                log::warn!(
                    "ignoring invalid retained execution context: session_id={} error={error:#}",
                    record.metadata.session_id
                );
                Vec::new()
            }),
    }
}

impl Config {
    pub fn use_session(&mut self, session_name: Option<&str>) -> Result<()> {
        let name =
            session_name.context("new session IDs must be reserved asynchronously in NATS")?;
        let mut session = self::session::new(self, name, None)?;
        let mut new_session = false;
        if session.is_empty() {
            new_session = true;
            if let Some(LastMessage {
                input,
                output,
                continuous,
            }) = &self.last_message
            {
                if (*continuous && !output.is_empty()) && self.agent.is_some() == input.with_agent()
                {
                    let ans = Confirm::new(
                        "Start a session that incorporates the last question and answer?",
                    )
                    .with_default(false)
                    .prompt()?;
                    if ans {
                        crate::config::session::add_assistant_text(
                            &mut session,
                            input,
                            output,
                            None,
                        )?;
                    }
                }
            }
        }
        let previous = self.session.replace(session);
        if let Err(error) = self.init_agent_session_variables(new_session) {
            self.session = previous;
            return Err(error);
        }
        if previous.is_some() {
            self.discontinuous_last_message();
        }
        Ok(())
    }

    pub fn session_info(&self) -> Result<String> {
        if let Some(session) = &self.session {
            self::session::render(session)
        } else {
            bail!("No session")
        }
    }

    pub fn exit_session(&mut self) -> Result<()> {
        if self.session.take().is_some() {
            self.discontinuous_last_message();
        }
        Ok(())
    }

    pub fn set_tui_editor_hooks(
        &mut self,
        before: Option<Box<dyn FnMut() + Send + Sync>>,
        after: Option<Box<dyn FnMut() + Send + Sync>>,
    ) {
        self.tui_before_editor = before;
        self.tui_after_editor = after;
    }

    /// Install (or clear) the TUI's native tool-confirmation callback. See
    /// `Config::tui_confirm_tool_use`.
    pub fn set_tui_confirm_tool_use(
        &mut self,
        confirm: Option<std::sync::Arc<crate::tool::ConfirmToolUseFn>>,
    ) {
        self.tui_confirm_tool_use = confirm;
    }

    pub fn empty_session(&mut self) -> Result<()> {
        if let Some(session) = self.session.as_mut() {
            if let Some(agent) = self.agent.as_ref() {
                session.sync_agent(agent)?;
            }
            crate::config::session::clear_messages(session)?;
        } else {
            bail!("No session")
        }
        self.discontinuous_last_message();
        Ok(())
    }

    pub(crate) fn empty_session_after_persisted_clear(&mut self) -> Result<()> {
        let session = self.session.as_mut().context("No session")?;
        crate::config::session::clear_messages_after_persisted_clear(session);
        self.discontinuous_last_message();
        Ok(())
    }

    pub(crate) fn reset_session_after_persisted_clear(&mut self) -> Result<()> {
        // Capture current session name before exiting
        let old_session_name = self.session.as_ref().map(|s| s.id().to_string());

        // Discard the current session without saving
        if let Some(session) = self.session.take() {
            drop(session);
            self.discontinuous_last_message();
        }
        if let Some(agent) = self.agent.as_mut() {
            agent.exit_session();
        }

        // Re-create previous session after agent changes.
        let session_name = old_session_name;
        if let Some(name) = session_name {
            self.use_session(Some(&name))?;
        }
        Ok(())
    }

    /// List remote sessions for a specific NATS cluster.
    ///
    /// # Returns
    ///
    /// - `Ok(vec![])` when the session metadata bucket exists but is empty.
    ///
    /// - `Err(...)` for actual fetch failures: connection refused, auth/permission denied,
    ///   network timeouts, or errors listing records from an existing bucket. The error
    ///   message includes the cluster name and root cause for user-facing diagnostics.
    ///
    /// This distinction allows callers to differentiate "no remote sessions" from
    /// "could not reach the cluster" — important for CLI error reporting and TUI visibility.
    pub async fn list_remote_sessions_with_meta(&self, cluster: &str) -> Result<Vec<SessionMeta>> {
        tokio::time::timeout(
            REMOTE_SESSION_LIST_TIMEOUT,
            self.list_remote_sessions_with_meta_inner(cluster),
        )
        .await
        .with_context(|| {
            format!(
                "Timed out listing sessions from NATS cluster '{cluster}' after {}s",
                REMOTE_SESSION_LIST_TIMEOUT.as_secs()
            )
        })?
    }

    async fn list_remote_sessions_with_meta_inner(
        &self,
        cluster: &str,
    ) -> Result<Vec<SessionMeta>> {
        let jetstream = match self.nats_jetstream(cluster).await {
            Ok(js) => js,
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("Failed to connect to NATS cluster '{cluster}' for session enumeration")
                });
            }
        };

        let store = match jetstream.get_key_value(SESSION_METADATA_BUCKET).await {
            Ok(store) => store,
            Err(e) => {
                // A missing bucket does not prove that there are no sessions: transcript
                // streams may predate the hard protocol cut. Report discovery as
                // unavailable so clients do not mislabel that state as an authoritative
                // empty list.
                if kv_bucket_missing(&e) {
                    log::debug!(
                        "Session metadata bucket '{}' not found for cluster '{}' (session discovery unavailable)",
                        SESSION_METADATA_BUCKET,
                        cluster
                    );
                    anyhow::bail!(
                        "Session discovery is not available yet because canonical metadata for cluster '{cluster}' has not been initialized; try again shortly"
                    );
                }
                return Err(e).with_context(|| {
                    format!(
                        "Failed to access session metadata bucket '{}' for cluster '{cluster}'",
                        SESSION_METADATA_BUCKET
                    )
                });
            }
        };

        let metadata_store = SessionMetadataStore::from_store(store, jetstream.client().clone());
        let records = match metadata_store.list().await {
            Ok(records) => records,
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("Failed to list session metadata records for cluster '{cluster}'")
                });
            }
        };

        let metas: Vec<SessionMeta> = records
            .into_iter()
            .map(|record| listed_session_to_meta(&record))
            .collect();

        Ok(metas)
    }

    pub fn is_compacting_session(&self) -> bool {
        self.session
            .as_ref()
            .map(|v| v.compressing())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests_remote_sessions {
    use super::*;
    use crate::nats_session_metadata::{SessionActivity, SessionInitializer, SessionMetadata};
    use chrono::{TimeZone, Utc};
    use std::time::UNIX_EPOCH;

    #[test]
    fn canonical_metadata_maps_to_session_meta() {
        let mut metadata = SessionMetadata::new(
            "test-session-123",
            SessionInitializer::named("hephaestus", Default::default()),
        );
        metadata.title.value = Some("Canonical title".to_string());
        let record = ListedSession {
            metadata,
            metadata_revision: 7,
            activity: Some(SessionActivity {
                first_activation_at: None,
                last_activity_at: Utc.timestamp_opt(1_719_531_234, 0).unwrap(),
            }),
        };

        let meta = listed_session_to_meta(&record);

        assert_eq!(meta.id, "test-session-123");
        assert_eq!(meta.session_id.as_deref(), Some("test-session-123"));
        assert_eq!(meta.agent_name.as_deref(), Some("hephaestus"));
        assert_eq!(meta.title.as_deref(), Some("Canonical title"));
        assert!(meta.modified.is_some());
        let modified = meta.modified.unwrap();
        assert_eq!(
            modified.duration_since(UNIX_EPOCH).unwrap().as_secs(),
            1_719_531_234_u64
        );
    }
}
