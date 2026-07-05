//! Session lifecycle and editing methods extracted from config/mod.rs for code health.
use super::*;
use crate::nats_admin::kv_bucket_missing;
use crate::nats_session_index::{self, SessionIndexRecord};
use std::time::{Duration, UNIX_EPOCH};

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
        modified: UNIX_EPOCH.checked_add(Duration::from_secs(record.last_activity)),
    }
}

impl Config {
    pub fn use_session(&mut self, session_name: Option<&str>) -> Result<()> {
        if self.session.is_some() {
            self.exit_session()?;
        }
        let mut session;
        match session_name {
            None => {
                let short_id = self.new_session_id()?;
                session = Some(self::session::new(self, &short_id, None)?);
            }
            Some(name) => {
                let session_path = self.session_file(name);
                if !session_path.exists() {
                    session = Some(self::session::new(self, name, None)?);
                } else {
                    session = Some(self::session::load(self, name, &session_path)?);
                }
            }
        }
        let mut new_session = false;
        let sessions_dir = self.sessions_dir();
        if let Some(session) = session.as_mut() {
            // Store sessions_dir so the log file can be lazily initialized
            // on the first event (avoids creating empty files in tests).
            // Must be set before any add_message() call that triggers logging.
            session.set_sessions_dir(sessions_dir);
            if session.runtime.is_none() {
                let session_path = self.session_file(&session.id);
                session.runtime = Some(Arc::new(Arc::new(self::session::FileSessionLogSink::new(
                    &session_path,
                    &session.id,
                    session.build_header_entry(),
                ))
                    as Arc<dyn self::session::SessionAppendSink>));
            }
            if session.is_empty() {
                new_session = true;
                if let Some(LastMessage {
                    input,
                    output,
                    continuous,
                }) = &self.last_message
                {
                    if (*continuous && !output.is_empty())
                        && self.agent.is_some() == input.with_agent()
                    {
                        let ans = Confirm::new(
                            "Start a session that incorporates the last question and answer?",
                        )
                        .with_default(false)
                        .prompt()?;
                        if ans {
                            crate::config::session::add_assistant_text(
                                session, input, output, None,
                            )?;
                        }
                    }
                }
            }
        }
        self.session = session;
        self.init_agent_session_variables(new_session)?;
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
        if let Some(mut session) = self.session.take() {
            let sessions_dir = self.sessions_dir();
            self::session::exit(&mut session, &sessions_dir, self.working_mode.is_tui())?;
            self.discontinuous_last_message();
        }
        Ok(())
    }

    pub fn save_session(&mut self, name: Option<&str>) -> Result<()> {
        let session_name = match &self.session {
            Some(session) => match name {
                Some(v) => v.to_string(),
                None => session.id().to_string(),
            },
            None => bail!("No session"),
        };
        let session_path = self.session_file(&session_name);
        if let Some(session) = self.session.as_mut() {
            self::session::save(
                session,
                &session_name,
                &session_path,
                self.working_mode.is_tui(),
            )?;
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

    pub fn edit_message_range(&mut self, from: usize, to: usize) -> Result<()> {
        let name = match &self.session {
            Some(session) => session.id().to_string(),
            None => bail!("No session"),
        };
        let session_path = self.session_file(&name);

        let raw_log = std::fs::read_to_string(&session_path)
            .with_context(|| format!("Failed to read '{}'", session_path.display()))?;
        let documents = split_session_log_documents(&raw_log);
        let entries = documents
            .iter()
            .map(|document| serde_yaml::from_str::<SessionLogEntry>(document))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let (from, to) =
            self::session_ops_core::compute_delete_range(from, to, &entries, &documents)?;

        // Replacement list order becomes new order for edited range. Reordering
        // plain message entries in editor is supported as long as edited YAML still
        // passes structural validation (including tool-call/result pairing).
        let selected_documents = documents[from..=to].to_vec();
        let temp_file = if let Some(ref dir) = self.temp_dir_override {
            dir.join(format!("message-edit-{}.yaml", uuid::Uuid::new_v4()))
        } else {
            temp_file("message-edit", ".yaml")
        };

        std::fs::write(&temp_file, selected_documents.join("\n---\n"))
            .with_context(|| format!("Failed to write to '{}'", temp_file.display()))?;

        let edit_result = self.edit_with_tui_hooks(|this| {
            let editor = this.editor()?;
            edit_file(&editor, &temp_file).with_context(|| {
                format!("Failed to edit '{}' with '{}'", temp_file.display(), editor)
            })
        });
        let edited_content = std::fs::read_to_string(&temp_file)
            .with_context(|| format!("Failed to read '{}'", temp_file.display()));
        edit_result?;
        let edited_content = edited_content?;

        let edited_documents = validate_edited_session_documents(&edited_content)?;
        validate_tool_pair_integrity(from, &edited_documents)?;

        let _ = std::fs::remove_file(&temp_file);

        let edit_entry = SessionLogEntry::EditEntries {
            from,
            to,
            replacements: edited_documents,
        };
        let session = self.session.as_mut().context("No session")?;
        if !crate::config::session::append_event(session, &edit_entry) {
            bail!("Failed to append session edit entry")
        }
        self.session = Some(self::session::load(self, &name, &session_path)?);
        self.discontinuous_last_message();
        Ok(())
    }

    /// Return the raw YAML documents for sequence numbers `from_seq..=to_seq`
    /// exactly as `.edit message` would open them in the editor, including
    /// auto-expansion for tool-call/result pairs.  Returns `None` when there is
    /// no active session, the session file cannot be read, or the range is
    /// invalid.  Documents are joined with `\n---\n`.
    pub fn get_message_range_yaml(&self, from_seq: usize, to_seq: usize) -> Option<String> {
        let session = self.session.as_ref()?;
        let session_path = self.session_file(session.id());
        let raw_log = std::fs::read_to_string(&session_path).ok()?;
        let documents = split_session_log_documents(&raw_log);
        if from_seq == 0 || to_seq >= documents.len() {
            return None;
        }
        let (from, to) = adjust_range_for_tool_pairs(from_seq, to_seq, &documents).ok()?;
        if from > to || to >= documents.len() {
            return None;
        }
        Some(documents[from..=to].join("\n---\n"))
    }

    pub fn delete_message_range(&mut self, from: usize, to: usize) -> Result<()> {
        let name = match &self.session {
            Some(session) => session.id().to_string(),
            None => bail!("No session"),
        };
        let session_path = self.session_file(&name);

        let raw_log = std::fs::read_to_string(&session_path)
            .with_context(|| format!("Failed to read '{}'", session_path.display()))?;
        let documents = split_session_log_documents(&raw_log);
        let entries = documents
            .iter()
            .map(|document| serde_yaml::from_str::<SessionLogEntry>(document))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let (from, to) =
            self::session_ops_core::compute_delete_range(from, to, &entries, &documents)?;

        let edit_entry = SessionLogEntry::EditEntries {
            from,
            to,
            replacements: vec![],
        };
        let session = self.session.as_mut().context("No session")?;
        if !crate::config::session::append_event(session, &edit_entry) {
            bail!("Failed to append session delete entry")
        }
        self.session = Some(self::session::load(self, &name, &session_path)?);
        self.discontinuous_last_message();
        Ok(())
    }

    pub fn rewind_session(&mut self, after_seq: usize) -> Result<()> {
        let name = match &self.session {
            Some(session) => session.id().to_string(),
            None => bail!("No session"),
        };
        let session_path = self.session_file(&name);

        let session = self.session.as_ref().context("No session")?;
        if after_seq >= session.log_entry_count {
            bail!(
                "Sequence number {} is out of range (log has {} entries)",
                after_seq,
                session.log_entry_count
            );
        }

        let raw_log = std::fs::read_to_string(&session_path)
            .with_context(|| format!("Failed to read '{}'", session_path.display()))?;
        let documents = split_session_log_documents(&raw_log);
        let entries = documents
            .iter()
            .map(|document| serde_yaml::from_str::<SessionLogEntry>(document))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let after_seq = self::session_ops_core::compute_rewind_point(
            after_seq,
            session.log_entry_count,
            &entries,
        )?;

        let rewind_entry = SessionLogEntry::Rewind { after_seq };
        let session = self.session.as_mut().context("No session")?;
        if !crate::config::session::append_event(session, &rewind_entry) {
            bail!("Failed to append session rewind entry")
        }
        self.session = Some(self::session::load(self, &name, &session_path)?);
        self.discontinuous_last_message();
        Ok(())
    }

    pub fn edit_session(&mut self) -> Result<()> {
        let name = match &self.session {
            Some(session) => session.id().to_string(),
            None => bail!("No session"),
        };
        let session_path = self.session_file(&name);
        self.save_session(Some(&name))?;
        self.edit_with_tui_hooks(|this| {
            let editor = this.editor()?;
            edit_file(&editor, &session_path).with_context(|| {
                format!(
                    "Failed to edit '{}' with '{editor}'",
                    session_path.display()
                )
            })
        })?;
        self.session = Some(self::session::load(self, &name, &session_path)?);
        self.discontinuous_last_message();
        Ok(())
    }

    pub fn empty_session(&mut self) -> Result<()> {
        if let Some(session) = self.session.as_mut() {
            if let Some(agent) = self.agent.as_ref() {
                session.sync_agent(agent)?;
                // Persist the updated agent name/prompt/variables to disk
                // before clearing messages, so the header reflects the
                // current agent state if the session file is reloaded.
                crate::config::session::append_event(session, &session.build_header_entry());
            }
            crate::config::session::clear_messages(session);
        } else {
            bail!("No session")
        }
        self.discontinuous_last_message();
        Ok(())
    }

    pub fn reset_session(&mut self) -> Result<()> {
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
            // Delete the persisted session file so use_session creates a fresh empty session
            // instead of reloading the old transcript. This ensures only the session ID
            // is preserved across agent changes, not the conversation history.
            let session_path = self.session_file(&name);
            if session_path.exists() {
                if let Err(err) = crate::config::attachments::remove_attachments_dir(&session_path)
                {
                    log::warn!(
                        "failed to remove attachments for '{}' during reset: {err}",
                        session_path.display()
                    );
                }
                remove_file(&session_path).with_context(|| {
                    format!("Failed to remove session file '{}'", session_path.display())
                })?;
            }
            self.use_session(Some(&name))?;
        }
        Ok(())
    }

    pub fn set_save_session_this_time(&mut self) -> Result<()> {
        if let Some(session) = self.session.as_mut() {
            session.set_save_session_this_time();
        } else {
            bail!("No session")
        }
        Ok(())
    }

    pub fn list_sessions(&self) -> Vec<String> {
        list_file_names(self.sessions_dir(), ".yaml")
    }

    pub fn list_sessions_with_meta(&self) -> Vec<SessionMeta> {
        let sessions_dir = self.sessions_dir();
        let mut sessions = Vec::new();
        Self::collect_session_metas(&sessions_dir, &sessions_dir, &mut sessions);
        sessions.sort_unstable_by(|left, right| left.id.cmp(&right.id));
        sessions
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

    fn collect_session_metas(sessions_dir: &Path, dir: &Path, out: &mut Vec<SessionMeta>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // Use entry.file_type() (symlink-aware, does not follow symlinks) to
            // avoid infinite recursion through symlinked directories.
            let file_type = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if file_type.is_dir() {
                Self::collect_session_metas(sessions_dir, &path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("yaml") {
                if let Ok(rel) = path.strip_prefix(sessions_dir) {
                    let id_path = rel.with_extension("");
                    let id = id_path
                        .components()
                        .map(|c| c.as_os_str().to_string_lossy())
                        .collect::<Vec<_>>()
                        .join("/");
                    if let Some(meta) = parse_session_meta(&id, &path) {
                        out.push(meta);
                    }
                }
            }
        }
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
            last_activity: u64::MAX,
        };

        let meta = session_index_record_to_meta(&record);

        assert!(meta.modified.is_none());
    }
}
