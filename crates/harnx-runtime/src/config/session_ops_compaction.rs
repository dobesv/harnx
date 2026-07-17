use super::*;
use crate::config::session_lock::SessionLock;

/// Rendered transcript of the messages to compact, plus the split index, the
/// covered log-seq range `(from, to, count)`, and the session id.
type CompactionTranscript = (String, usize, (Option<usize>, Option<usize>, usize), String);

impl Config {
    pub fn maybe_compact_session(config: GlobalConfig) {
        let mut need_compact = false;
        let mut msg_count = 0usize;
        let mut already_compacting = false;
        {
            let mut config = config.write();
            let compress_threshold = config.compress_threshold;
            if let Some(session) = config.session.as_mut() {
                if session.need_compress(compress_threshold) {
                    already_compacting = session.compressing();
                    msg_count = session.messages.len();
                    session.set_compressing(true);
                    need_compact = true;
                }
            }
        };
        if !need_compact {
            return;
        }
        if already_compacting {
            log::warn!(
                "compaction: triggered while a previous compaction is still in \
                 progress (messages={msg_count}) — overlapping compaction tasks"
            );
        }
        log::info!("compaction: started (messages={msg_count})");
        let started = std::time::Instant::now();
        let compacting_session_id = config
            .read()
            .session
            .as_ref()
            .map(|session| session.id.clone());
        harnx_core::sink::emit_agent_event(harnx_core::event::AgentEvent::Session(
            harnx_core::event::SessionEvent::CompactingStarted,
        ));
        tokio::spawn(async move {
            let result = Config::compact_session(&config).await;
            if let Some(compacting_session_id) = compacting_session_id.as_deref() {
                if let Some(session) = config.write().session.as_mut() {
                    if session.id == compacting_session_id {
                        session.set_compressing(false);
                    }
                }
            }
            match &result {
                Ok(()) => {
                    log::info!(
                        "compaction: completed in {:?} (messages_before={msg_count})",
                        started.elapsed()
                    );
                    harnx_core::sink::emit_agent_event(harnx_core::event::AgentEvent::Session(
                        harnx_core::event::SessionEvent::CompactingCompleted,
                    ));
                }
                Err(err) => {
                    warn!(
                        "Failed to compact the session after {:?}: {err}",
                        started.elapsed()
                    );
                    harnx_core::sink::emit_agent_event(harnx_core::event::AgentEvent::Session(
                        harnx_core::event::SessionEvent::CompactingFailed(err.to_string()),
                    ));
                }
            }
        });
    }

    pub async fn compact_session(config: &GlobalConfig) -> Result<()> {
        Self::ensure_compactable(config)?;

        let summarizer_agent = match Self::resolve_compaction_agent(config) {
            Some(agent) => agent.into_config(),
            None => harnx_core::agent_config::AgentConfig::from_prompt(
                crate::config::compaction::DEFAULT_COMPACT_SYSTEM_PROMPT,
            ),
        };
        let params = crate::config::compaction::compaction_params(&summarizer_agent);

        let (transcript, split, covered, session_id) =
            Self::build_compaction_transcript(config, &params)?;

        // Derive session path BEFORE making LLM call, to check if we need lock.
        let session_path = {
            let guard = config.read();
            let Some(session) = guard.session.as_ref() else {
                return Ok(()); // No session to compact
            };
            if session.id != session_id {
                return Ok(()); // Session swapped
            }
            // Skip locking for ephemeral/unsaved sessions
            if session.save_session() == Some(false) {
                None
            } else {
                match (&session.path, &session.sessions_dir) {
                    (Some(path), _) => Some(std::path::PathBuf::from(path)),
                    (None, Some(dir)) => Some(dir.join(format!("{}.yaml", session.id))),
                    (None, None) => None,
                }
            }
        };

        let mut input = harnx_core::input::Input::new(
            transcript.clone(),
            (transcript, vec![]),
            summarizer_agent,
        );
        input.with_session = false;
        input.with_agent = true;

        let summary = crate::config::input::fetch_chat_text(&mut input, config).await?;

        let summary_with_note = append_recovery_note(summary, covered);

        // Apply compaction with proper locking to avoid race/deadlock.
        // This runs on a background task that may start BEFORE the agent loop's
        // SessionLock is released. Use bounded try_acquire to avoid deadlock.
        if let Some(session_path) = session_path {
            // Try to acquire the session lock with bounded retries.
            let lock = {
                let session_path = session_path.clone();
                tokio::task::spawn_blocking(move || {
                    let mut attempts = 0;
                    let max_attempts = 20; // ~2s total with 100ms sleeps
                    let sleep_ms = 100;
                    loop {
                        match SessionLock::try_acquire(&session_path) {
                            Ok(Some(lock)) => return Ok(lock),
                            Ok(None) => {
                                // Lock held by another process; retry
                                attempts += 1;
                                if attempts >= max_attempts {
                                    return Err(anyhow::anyhow!(
                                        "compaction: could not acquire session lock after {} attempts",
                                        attempts
                                    ));
                                }
                                std::thread::sleep(std::time::Duration::from_millis(sleep_ms));
                            }
                            Err(e) => return Err(e),
                        }
                    }
                })
                .await
                .context("compaction lock task join")?
            };

            match lock {
                Ok(_session_lock) => {
                    // Lock acquired - apply compaction under lock.
                    Self::apply_compaction_summary(config, &session_id, summary_with_note, split);
                }
                Err(e) => {
                    // Failed to acquire lock - skip this compaction cycle.
                    // Will re-trigger on next turn.
                    log::warn!(
                        "compaction: could not acquire session lock for {}: {}; skipping this cycle",
                        session_id,
                        e
                    );
                }
            }
        } else {
            // No path - ephemeral session; apply in-memory compaction without log append.
            Self::apply_compaction_summary(config, &session_id, summary_with_note, split);
        }

        Ok(())
    }

    /// Validate that the current session exists and has user messages to compact.
    fn ensure_compactable(config: &GlobalConfig) -> Result<()> {
        let guard = config.read();
        let session = guard.session.as_ref().context("No session")?;
        if !session.has_user_messages() {
            bail!("No need to compact since there are no messages in the session");
        }
        Ok(())
    }

    /// Write the compaction summary back into the session, but only if the
    /// active session is still the one we compacted (it may have been swapped).
    /// Returns whether the summary was applied.
    pub(crate) fn apply_compaction_summary(
        config: &GlobalConfig,
        session_id: &str,
        summary_with_note: String,
        split: usize,
    ) -> bool {
        let mut guard = config.write();
        let Some(session) = guard.session.as_mut() else {
            return false;
        };
        if session.id != session_id {
            return false;
        }
        crate::config::session::compress_keeping_recent(session, summary_with_note, split);
        guard.discontinuous_last_message();
        true
    }

    /// Resolve the configured `compaction_agent` (if any) for the active agent,
    /// applying package-relative name resolution and variable interpolation.
    /// Returns `None` (use the default compaction prompt) when not configured or
    /// when the agent fails to load/resolve.
    fn resolve_compaction_agent(config: &GlobalConfig) -> Option<crate::config::agent::Agent> {
        let active_agent_name = config.read().extract_agent().name().to_string();
        let active_pkg = harnx_core::package_namespace::pkg_from_qualified(&active_agent_name);
        let name = config
            .read()
            .extract_agent()
            .compaction_agent()
            .map(str::to_owned)?;

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
    }

    /// Compute the prefix split point and render the transcript of messages to
    /// be compacted, along with the covered log-seq range and session id.
    fn build_compaction_transcript(
        config: &GlobalConfig,
        params: &crate::config::compaction::CompactionParams,
    ) -> Result<CompactionTranscript> {
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
        Ok((transcript, split, (from, to, prefix.len()), session_id))
    }
}

/// Append short recovery note describing compacted range so future
/// reader knows detail is recoverable from on-disk log.
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
