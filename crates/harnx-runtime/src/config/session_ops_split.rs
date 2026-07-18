//! Session lifecycle and editing methods extracted from config/mod.rs for code health.
use super::*;
use crate::config::session_lock::SessionLock;
use crate::nats_admin::kv_bucket_missing;
use crate::nats_session_index::{self, SessionIndexRecord};
use harnx_core::session_log::SessionLog;
use std::fs::read_to_string;
use std::sync::Arc;
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
        title: record.title.clone(),
        modified: UNIX_EPOCH.checked_add(Duration::from_secs(record.last_activity)),
    }
}

/// Called immediately after acquiring the session lock, to pick up entries
/// written by the previous lock holder. Reloads the session log from disk
/// and merges into the in-memory Session, preserving the existing runtime
/// append sink and sessions_dir. Also resets the sink's seq cache so the
/// next append re-derives from the freshly-reloaded file.
pub fn reload_session_from_disk(config: &GlobalConfig) -> Result<()> {
    let mut config = config.write();
    let Some(session) = config.session.as_ref() else {
        return Ok(());
    };
    let Some(path_str) = session.path.clone() else {
        return Ok(());
    };

    let path = PathBuf::from(&path_str);
    let name = session.id.clone();

    // Skip reload if the session file doesn't exist or is empty (uninitialized stub).
    // New sessions are claimed by creating a zero-byte stub; the first append will
    // initialize the header. Reloading from an empty file would fail with
    // "invalid type: Option value, expected internally tagged enum SessionLogEntry".
    if !path.exists() {
        return Ok(());
    }
    let metadata = path.metadata().with_context(|| {
        format!(
            "Failed to read metadata for session {} at {}",
            name,
            path.display()
        )
    })?;
    if metadata.len() == 0 {
        return Ok(());
    }

    // Preserve the exact Arc<dyn SessionAppendSink> — no re-wrapping.
    let runtime = session.runtime.clone();
    let sessions_dir = session.sessions_dir.clone();
    // Preserve the current in-memory model in case reload fails to resolve it.
    // This can happen when the model is a mock or dynamically-built client not
    // present in the static clients catalog (e.g., TUI tests with MockClient).
    let preserved_model = session.model.clone();
    let preserved_model_id = session.model_id.clone();

    let content = read_to_string(&path)
        .with_context(|| format!("Failed to load session {} at {}", name, path.display()))?;
    let mut reloaded = match self::session::load_from_log(&config, &name, &path, &content) {
        Ok(session) => session,
        Err(e) => {
            // If load_from_log failed due to model resolution but the in-memory
            // session has a valid model for the same model_id, try loading again
            // without model resolution and restore the preserved model.
            let is_model_error =
                e.to_string().contains("Unknown") && e.to_string().contains("model");
            if !is_model_error {
                return Err(e);
            }
            // Fall back to loading log entries without model resolution
            let log = self::session::FileSessionLog::new_for_reload(&path, &name);
            let raw_entries = log.load_events()?;
            let replay_entries: Vec<_> = raw_entries
                .iter()
                .map(|(seq, entry)| (*seq as usize, entry.clone()))
                .collect();
            let mut session =
                self::session::replay_log_entries_for_external(&replay_entries, &name)?;
            session.log_entry_count = raw_entries.len();
            self::session::apply_name_and_path(&mut session, &name, &path, &config)?;
            session.update_tokens();
            session
        }
    };

    // Reset the sink's seq cache so next append re-derives from the reloaded file.
    // Call through the SessionAppendSink trait method.
    if let Some(sink) = runtime
        .as_ref()
        .and_then(|r| r.downcast_ref::<Arc<dyn self::session::SessionAppendSink>>())
    {
        sink.reset_seq_cache();
    }

    reloaded.runtime = runtime;
    reloaded.sessions_dir = sessions_dir;
    // Preserve the in-memory model when the reloaded model_id matches the current one.
    // The running turn already holds a fully-resolved Model (possibly a mock or
    // dynamically-built client not resolvable from the static clients catalog).
    if reloaded.model_id == preserved_model_id {
        reloaded.model = preserved_model;
    }
    config.session = Some(reloaded);
    Ok(())
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
                let sink: Arc<dyn self::session::SessionAppendSink> =
                    Arc::new(self::session::FileSessionLogSink::new(
                        &session_path,
                        &session.id,
                        session.build_header_entry(),
                    ));
                // Double-wrap to match convention: Arc<dyn Any> wrapping Arc<dyn SessionAppendSink>.
                // This is required for append_event's downcast to work.
                session.runtime = Some(Arc::new(sink));
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
        self.exit_session_with_lock(None)
    }

    pub fn exit_session_with_lock(&mut self, lock: Option<&SessionLock>) -> Result<()> {
        if let Some(mut session) = self.session.take() {
            let sessions_dir = self.sessions_dir();
            self::session::exit(
                &mut session,
                &sessions_dir,
                self.working_mode.is_tui(),
                lock,
            )?;
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
                None,
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

        // Acquire session lock before appending edit entry.
        let _lock = SessionLock::acquire(&session_path)?;

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

        // Acquire session lock before appending delete entry.
        let _lock = SessionLock::acquire(&session_path)?;

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

        // Acquire session lock before appending rewind entry.
        let _lock = SessionLock::acquire(&session_path)?;

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
        self.empty_session_with_lock(None)
    }

    pub fn empty_session_with_lock(&mut self, lock: Option<&SessionLock>) -> Result<()> {
        let session_path = match self.session.as_ref() {
            Some(session) if session.save_session() == Some(false) => None,
            Some(session) => session
                .path
                .as_deref()
                .map(PathBuf::from)
                .or_else(|| {
                    session
                        .sessions_dir
                        .as_ref()
                        .map(|dir| dir.join(format!("{}.yaml", session.id)))
                })
                .or_else(|| Some(self.session_file(&session.id))),
            None => bail!("No session"),
        };
        // If caller already holds the lock (Some), don't re-acquire (File::lock is not re-entrant).
        // If None (standalone caller), acquire our own short-lived lock.
        let _lock = match (session_path.as_ref(), lock) {
            (_, Some(_)) => None, // Caller holds lock; don't reacquire
            (Some(session_path), None) => Some(SessionLock::acquire(session_path)?),
            (None, None) => None, // Ephemeral session; no lock
        };

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

    #[test]
    fn reload_session_from_disk_reloads_log_entry_count_from_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let session_path = tmp.path().join("reload-test.yaml");
        let content = r#"type: header
model: test:model
---
type: message
role: user
content: first
---
type: message
role: assistant
content: second
"#;
        std::fs::write(&session_path, content).unwrap();

        let mut config = Config {
            sessions_dir_override: Some(tmp.path().to_path_buf()),
            working_mode: WorkingMode::Cmd,
            ..Config::default()
        };
        config
            .clients
            .push(harnx_client::ClientConfig::OpenAICompatibleConfig(
                harnx_core::provider_config::openai_compatible::OpenAICompatibleConfig {
                    name: "test".to_string(),
                    api_base: None,
                    api_key: None,
                    models: vec![],
                    patches: None,
                    extra: None,
                    system_prompt_prefix: None,
                    package: None,
                },
            ));
        config.model = harnx_client::Model::new("test", "model");
        config.model_id = "test:model".to_string();
        let mut session = self::session::new(&config, "reload-test", None).unwrap();
        session.path = Some(session_path.display().to_string());
        session.set_sessions_dir(tmp.path().to_path_buf());
        session.log_entry_count = 1;
        // Create the inner sink: Arc<dyn SessionAppendSink>
        let inner_sink: Arc<dyn self::session::SessionAppendSink> =
            Arc::new(self::session::FileSessionLogSink::new(
                &session_path,
                &session.id,
                session.build_header_entry(),
            ));
        // Double-wrap to match convention: Arc<dyn Any> wrapping Arc<dyn SessionAppendSink>.
        let original_runtime: Arc<dyn std::any::Any + Send + Sync> = Arc::new(inner_sink);
        let original_ptr = Arc::as_ptr(&original_runtime) as *const ();
        session.runtime = Some(original_runtime);
        config.session = Some(session);
        let global_config: GlobalConfig = Arc::new(RwLock::new(config));

        super::reload_session_from_disk(&global_config).unwrap();

        let mut config = global_config.write();
        let session = config.session.as_mut().unwrap();
        assert_eq!(session.log_entry_count, 3);
        assert_eq!(
            session.path.as_deref(),
            Some(session_path.to_str().unwrap())
        );
        assert_eq!(session.sessions_dir.as_deref(), Some(tmp.path()));
        assert!(session.runtime.is_some());
        // Verify we preserved the exact same outer Arc<dyn Any> by comparing raw pointers
        let reloaded_runtime = session.runtime.as_ref().unwrap();
        let reloaded_ptr = Arc::as_ptr(reloaded_runtime) as *const ();
        assert_eq!(
            original_ptr, reloaded_ptr,
            "runtime Arc must be ptr_eq to original"
        );

        // REGRESSION GUARD: Append through the sink after reload, verify it uses the sink
        // (not the fallback path) and assigns correct seq derived from the reloaded file.
        let entry = self::session::SessionLogEntry::Title {
            title: "test title".to_string(),
            manual: true,
            tokens: 0,
        };
        let appended = self::session::append_event(session, &entry);
        assert!(appended, "append_event should use the sink after reload");
        // After appending 1 entry to a file with 3 entries, log_entry_count should be 4.
        assert_eq!(
            session.log_entry_count, 4,
            "seq should be derived from reloaded file (3 entries -> next seq 3 -> count 4)"
        );
    }
}
