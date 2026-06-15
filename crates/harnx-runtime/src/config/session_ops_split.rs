//! Session lifecycle and editing methods extracted from config/mod.rs for code health.
use super::*;

impl Config {
    pub fn use_session(&mut self, session_name: Option<&str>) -> Result<()> {
        if self.session.is_some() {
            self.exit_session()?;
        }
        let mut session;
        match session_name {
            None => {
                let short_id = self.new_session_id()?;
                session = Some(self::session::new(self, &short_id)?);
            }
            Some(name) => {
                let session_path = self.session_file(name);
                if !session_path.exists() {
                    session = Some(self::session::new(self, name)?);
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
        if from == 0 {
            bail!("Cannot edit or delete the session header (sequence 0)");
        }
        if to >= documents.len() {
            bail!("Sequence numbers out of range");
        }
        let (from, to) = adjust_range_for_tool_pairs(from, to, &documents)?;
        if from > to || to >= documents.len() {
            bail!("Sequence numbers out of range");
        }

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
        if from == 0 {
            bail!("Cannot edit or delete the session header (sequence 0)");
        }
        if to >= documents.len() {
            bail!("Sequence numbers out of range");
        }
        let (from, to) = adjust_range_for_tool_pairs(from, to, &documents)?;
        if from > to || to >= documents.len() {
            bail!("Sequence numbers out of range");
        }

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

        // Reject a cut point that splits a ToolCalls/ToolResults pair.
        let raw_log = std::fs::read_to_string(&session_path)
            .with_context(|| format!("Failed to read '{}'", session_path.display()))?;
        let documents = split_session_log_documents(&raw_log);
        let parse = |idx: usize| -> Option<SessionLogEntry> {
            documents
                .get(idx)
                .and_then(|raw| serde_yaml::from_str::<SessionLogEntry>(raw).ok())
        };
        if matches!(parse(after_seq), Some(SessionLogEntry::ToolCalls { .. }))
            && matches!(
                parse(after_seq + 1),
                Some(SessionLogEntry::ToolResults { .. })
            )
        {
            bail!(
                "Sequence {after_seq} is a tool-calls entry paired with tool-results at {}; \
                 rewinding here would orphan the tool calls. \
                 Use {} to keep the pair or {} to exclude it.",
                after_seq + 1,
                after_seq + 1,
                after_seq.saturating_sub(1),
            );
        }

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

    pub fn maybe_compact_session(config: GlobalConfig) {
        let mut need_compact = false;
        {
            let mut config = config.write();
            let compress_threshold = config.compress_threshold;
            if let Some(session) = config.session.as_mut() {
                if session.need_compress(compress_threshold) {
                    session.set_compressing(true);
                    need_compact = true;
                }
            }
        };
        if !need_compact {
            return;
        }
        // Use SessionEvent for consistent TUI/CLI handling.
        // The TUI will render CompactingStarted/CompactingCompleted as transcript items.
        harnx_core::sink::emit_agent_event(harnx_core::event::AgentEvent::Session(
            harnx_core::event::SessionEvent::CompactingStarted,
        ));
        tokio::spawn(async move {
            let result = Config::compact_session(&config).await;
            if let Some(session) = config.write().session.as_mut() {
                session.set_compressing(false);
            }
            match &result {
                Ok(()) => {
                    harnx_core::sink::emit_agent_event(harnx_core::event::AgentEvent::Session(
                        harnx_core::event::SessionEvent::CompactingCompleted,
                    ));
                }
                Err(err) => {
                    warn!("Failed to compact the session: {err}");
                    harnx_core::sink::emit_agent_event(harnx_core::event::AgentEvent::Session(
                        harnx_core::event::SessionEvent::CompactingFailed(err.to_string()),
                    ));
                }
            }
        });
    }

    pub async fn compact_session(config: &GlobalConfig) -> Result<()> {
        match config.read().session.as_ref() {
            Some(session) => {
                if !session.has_user_messages() {
                    bail!("No need to compact since there are no messages in the session")
                }
            }
            None => bail!("No session"),
        }

        // Check if the current agent has a compaction_agent configured
        let active_agent_name = config.read().extract_agent().name().to_string();
        let active_pkg = harnx_core::package_namespace::pkg_from_qualified(&active_agent_name);
        let compaction_agent_name = config
            .read()
            .extract_agent()
            .compaction_agent()
            .map(str::to_owned);

        let agent_override = if let Some(name) = compaction_agent_name {
            let resolved_name =
                harnx_core::package_namespace::resolve_package_relative_name(&name, active_pkg);
            match config.read().retrieve_agent(&resolved_name) {
                Ok(mut compaction_agent) => {
                    if let Err(e) = self::agent::resolve_variables(&mut compaction_agent) {
                        warn!("Failed to resolve variables for compaction_agent '{name}': {e}");
                    }
                    Some(compaction_agent)
                }
                Err(e) => {
                    warn!(
                        "Failed to load compaction_agent '{name}': {e}; falling back to default compaction"
                    );
                    None
                }
            }
        } else {
            None
        };

        // Build the summarizer agent up front (configured compaction_agent, or
        // the synthetic default), and resolve the compaction tuning params from
        // it so the snapshot below can use them.
        let summarizer_agent = match agent_override {
            Some(agent) => agent.into_config(),
            None => harnx_core::agent_config::AgentConfig::from_prompt(
                crate::config::compaction::DEFAULT_COMPACT_SYSTEM_PROMPT,
            ),
        };
        let params = crate::config::compaction::compaction_params(&summarizer_agent);

        // 1. Snapshot what we need under a read lock: the transcript of the prefix
        //    to compact, the split point, and the covered log-seq range.
        let (transcript, split, covered, session_id) = {
            let guard = config.read();
            let session = guard.session.as_ref().context("No session")?;
            let session_id = session.id.clone();
            let model = session.model().clone();
            let split = crate::config::compaction::split_index(
                &session.messages,
                &model,
                params.keep_recent_turns,
                params.keep_recent_tokens,
            );
            if split == 0 {
                bail!("Nothing to compact");
            }
            let prefix = &session.messages[..split];
            let transcript =
                crate::config::compaction::render_transcript(prefix, params.tool_output_max_chars);
            let from = prefix.iter().filter_map(|m| m.log_seq).min();
            let to = prefix.iter().filter_map(|m| m.log_seq).max();
            (transcript, split, (from, to, prefix.len()), session_id)
        };

        // 2. Build a controlled summarization request: summarizer system prompt +
        //    the transcript as the user message, WITHOUT the live session.
        let mut input = harnx_core::input::Input::new(
            transcript.clone(),
            (transcript, vec![]),
            summarizer_agent,
        );
        input.with_session = false;
        input.with_agent = true;

        let summary = crate::config::input::fetch_chat_text(&input, config).await?;

        // 3. Append a recovery note and store, keeping the recent suffix verbatim.
        let summary_with_note = append_recovery_note(summary, covered);
        if let Some(session) = config.write().session.as_mut() {
            if session.id == session_id {
                crate::config::session::compress_keeping_recent(session, summary_with_note, split);
            }
        }
        config.write().discontinuous_last_message();
        Ok(())
    }

    pub fn is_compacting_session(&self) -> bool {
        self.session
            .as_ref()
            .map(|v| v.compressing())
            .unwrap_or_default()
    }
}

/// Append a short recovery note describing the compacted range so a future
/// reader knows the detail is recoverable from the on-disk log.
fn append_recovery_note(summary: String, covered: (Option<usize>, Option<usize>, usize)) -> String {
    let (from, to, count) = covered;
    let range = match (from, to) {
        (Some(a), Some(b)) => format!(" (log entries {a}–{b})"),
        _ => String::new(),
    };
    format!(
        "{summary}\n\n[Earlier conversation: {count} message(s){range} were summarized above. \
The full pre-compaction transcript remains in this session's log; use the \
`harnx_agent_session_history_read` tool to search it by entry index, type, tool name, or text.]"
    )
}
