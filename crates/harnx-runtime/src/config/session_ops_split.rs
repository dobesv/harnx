//! Session lifecycle and editing methods extracted from config/mod.rs for code health.
use super::*;
use crate::nats_admin::kv_bucket_missing;
use crate::nats_session_index::{self, SessionIndexRecord};
use std::time::{Duration, UNIX_EPOCH};

const REMOTE_SESSION_LIST_TIMEOUT: Duration = Duration::from_secs(10);

/// Convert a `SessionIndexRecord` to `SessionMeta`.
///
/// This mapping is used by `list_remote_sessions_with_meta` to translate
/// NATS KV index records into the picker/CLI-friendly `SessionMeta` format.
fn session_index_record_to_meta(record: &SessionIndexRecord) -> SessionMeta {
    SessionMeta {
        id: record.session_id.clone(),
        session_id: Some(record.session_id.clone()),
        working_dir: record.working_dir.clone(),
        git_branch: record.git_branch.clone(),
        git_remote: record.git_remote.clone(),
        terminal_session_id: None,
        agent_name: Some(record.agent_name.clone()),
        title: record.title.clone(),
        modified: UNIX_EPOCH.checked_add(Duration::from_secs(record.last_activity)),
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
                if !crate::config::session::append_event(session, &session.build_header_entry()) {
                    bail!("Failed to persist session header before clearing")
                }
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
    /// - `Ok(vec![])` when the session index bucket does not exist or exists but is empty.
    ///   These are legitimate "no sessions indexed yet" cases and do not indicate failure.
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

        let store = match jetstream
            .get_key_value(nats_session_index::SESSION_INDEX_BUCKET)
            .await
        {
            Ok(store) => store,
            Err(e) => {
                // Bucket not found -> empty result (no sessions indexed yet).
                // All other errors (auth, network, permissions) -> propagate as Err.
                if kv_bucket_missing(&e) {
                    log::debug!(
                        "Session index bucket '{}' not found for cluster '{}' (no sessions indexed yet)",
                        nats_session_index::SESSION_INDEX_BUCKET,
                        cluster
                    );
                    return Ok(vec![]);
                }
                return Err(e).with_context(|| {
                    format!(
                        "Failed to access session index bucket '{}' for cluster '{cluster}'",
                        nats_session_index::SESSION_INDEX_BUCKET
                    )
                });
            }
        };

        let records = match nats_session_index::list_records(&store).await {
            Ok(records) => records,
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("Failed to list session index records for cluster '{cluster}'")
                });
            }
        };

        let mut metas: Vec<SessionMeta> = records
            .into_iter()
            .map(|record| session_index_record_to_meta(&record))
            .collect();
        metas.sort_unstable_by(|left, right| left.id.cmp(&right.id));

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

    #[test]
    fn test_session_index_record_to_meta_mapping() {
        let record = SessionIndexRecord {
            session_id: "test-session-123".to_string(),
            agent_name: "hephaestus".to_string(),
            working_dir: Some("/home/user/project".to_string()),
            git_branch: Some("feature-branch".to_string()),
            git_remote: Some("git@github.com:org/repo.git".to_string()),
            title: None,
            last_activity: 1_719_531_234,
        };

        let meta = session_index_record_to_meta(&record);

        assert_eq!(meta.id, "test-session-123");
        assert_eq!(meta.session_id.as_deref(), Some("test-session-123"));
        assert_eq!(meta.working_dir.as_deref(), Some("/home/user/project"));
        assert_eq!(meta.git_branch.as_deref(), Some("feature-branch"));
        assert_eq!(
            meta.git_remote.as_deref(),
            Some("git@github.com:org/repo.git")
        );
        assert!(meta.agent_name.is_some());
        assert_eq!(meta.agent_name.as_deref(), Some("hephaestus"));
        assert!(meta.terminal_session_id.is_none());
        assert!(meta.modified.is_some());
        let modified = meta.modified.unwrap();
        assert_eq!(
            modified.duration_since(UNIX_EPOCH).unwrap().as_secs(),
            1_719_531_234_u64
        );
    }

    #[test]
    fn test_session_index_record_to_meta_minimal_fields() {
        let record = SessionIndexRecord {
            session_id: "minimal-session".to_string(),
            agent_name: "default".to_string(),
            working_dir: None,
            git_branch: None,
            git_remote: None,
            title: None,
            last_activity: 0,
        };

        let meta = session_index_record_to_meta(&record);

        assert_eq!(meta.id, "minimal-session");
        assert_eq!(meta.session_id.as_deref(), Some("minimal-session"));
        assert!(meta.working_dir.is_none());
        assert!(meta.git_branch.is_none());
        assert!(meta.git_remote.is_none());
        assert_eq!(meta.agent_name.as_deref(), Some("default"));
        assert!(meta.terminal_session_id.is_none());
        assert_eq!(
            meta.modified
                .unwrap()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            0_u64
        );
    }

    #[test]
    fn test_session_index_record_to_meta_overflow_yields_none_modified() {
        let record = SessionIndexRecord {
            session_id: "overflow-session".to_string(),
            agent_name: "hephaestus".to_string(),
            working_dir: None,
            git_branch: None,
            git_remote: None,
            title: None,
            last_activity: u64::MAX,
        };

        let meta = session_index_record_to_meta(&record);

        assert!(meta.modified.is_none());
    }
}
