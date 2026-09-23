use super::*;
use std::io::Write;

use crate::utils::AbortSignal;

/// Rendered transcript of the messages to compact, plus the split index, the
/// covered log-seq range `(from, to, count)`, and the session id.
type CompactionTranscript = (String, usize, (Option<usize>, Option<usize>, usize), String);

enum CompactionClaim {
    Claimed,
    AlreadyCompacting,
    NoSession,
}

pub(crate) async fn handle_compact_session_command(
    config: &GlobalConfig,
    abort_signal: &AbortSignal,
    output: &mut (dyn Write + Send),
) -> Result<()> {
    // Determine if this is a NATS-backed session by checking if remote_agent is set
    // or if nats_servers are configured (mirrors logic in remote_nats_session).
    let is_nats_session = {
        let cfg = config.read();
        cfg.remote_agent.is_some() || !cfg.nats_servers.is_empty()
    };

    if is_nats_session {
        // For NATS-backed sessions, route through remote_nats_session.
        // If that fails (e.g., NATS connection error), emit the error.
        match remote_session_ops::remote_nats_session(config, abort_signal).await {
            Ok(session) => emit_compaction_submit_result(session.request_compaction(None).await),
            Err(error) => {
                emit_compaction_submit_result(Err(error));
            }
        }
    } else {
        // Only fall back to local compaction for truly local (in-memory) sessions.
        compact_local_session(config, output).await?;
    }
    Ok(())
}

fn emit_compaction_submit_result(result: Result<crate::nats_session::CompactSubmit>) {
    use crate::nats_session::CompactSubmit;
    use harnx_core::event::{AgentEvent, SessionEvent};

    let event = match result {
        Ok(CompactSubmit::Submitted { .. }) => return,
        Ok(CompactSubmit::AlreadyInFlight { .. }) => SessionEvent::Generic {
            text: "Compaction already in progress".to_string(),
        },
        Ok(CompactSubmit::NothingToDo { outcome }) => SessionEvent::Generic {
            text: compact_outcome_message(outcome).to_string(),
        },
        Err(error) => SessionEvent::CompactingFailed {
            compaction_id: None,
            error: error.to_string(),
        },
    };
    harnx_core::sink::emit_agent_event(AgentEvent::Session(event));
}

fn compact_outcome_message(outcome: harnx_core::session::CompactOutcome) -> &'static str {
    use harnx_core::session::{CompactOutcome, UnchangedReason};

    match outcome {
        CompactOutcome::Compacted | CompactOutcome::Failed(_) => "Nothing to compact",
        CompactOutcome::Unchanged(UnchangedReason::NoUserMessages) => "No user messages to compact",
        CompactOutcome::Unchanged(UnchangedReason::NothingEligible) => {
            "Nothing eligible for compaction"
        }
        CompactOutcome::Unchanged(UnchangedReason::AlreadyCompacted) => "Session already compacted",
    }
}

async fn compact_local_session(
    config: &GlobalConfig,
    output: &mut (dyn Write + Send),
) -> Result<()> {
    use harnx_core::event::{AgentEvent, SessionEvent};
    use harnx_core::session::CompactOutcome;

    match claim_local_compaction(config) {
        CompactionClaim::NoSession => {
            writeln!(output, "No active session to compact.")?;
            return Ok(());
        }
        CompactionClaim::AlreadyCompacting => {
            writeln!(output, "Compaction already in progress.")?;
            return Ok(());
        }
        CompactionClaim::Claimed => {}
    }

    harnx_core::sink::emit_agent_event(AgentEvent::Session(SessionEvent::CompactingStarted {
        compaction_id: None,
    }));
    let result = Config::compact_session(config).await;
    if let Some(session) = config.write().session.as_mut() {
        session.set_compressing(false);
    }

    let event = match result {
        Ok(()) => SessionEvent::CompactingCompleted {
            compaction_id: None,
            outcome: CompactOutcome::Compacted,
        },
        Err(error) => match classify_compaction_error(&error) {
            CompactOutcome::Failed(_) => SessionEvent::CompactingFailed {
                compaction_id: None,
                error: error.to_string(),
            },
            outcome => SessionEvent::CompactingCompleted {
                compaction_id: None,
                outcome,
            },
        },
    };
    harnx_core::sink::emit_agent_event(AgentEvent::Session(event));
    Ok(())
}

fn claim_local_compaction(config: &GlobalConfig) -> CompactionClaim {
    match config.write().session.as_mut() {
        None => CompactionClaim::NoSession,
        Some(session) if session.compressing() => CompactionClaim::AlreadyCompacting,
        Some(session) => {
            session.set_compressing(true);
            CompactionClaim::Claimed
        }
    }
}

/// Classify a compaction error into an outcome for event emission.
/// Used by both the TUI command handler and the worker-side turn handler.
pub fn classify_compaction_error(err: &anyhow::Error) -> harnx_core::session::CompactOutcome {
    let msg = format!("{err:#}");
    if msg.contains("No need to compact") || msg.contains("no messages in the session") {
        harnx_core::session::CompactOutcome::Unchanged(
            harnx_core::session::UnchangedReason::NoUserMessages,
        )
    } else if msg.contains("Nothing to compact") {
        harnx_core::session::CompactOutcome::Unchanged(
            harnx_core::session::UnchangedReason::NothingEligible,
        )
    } else {
        harnx_core::session::CompactOutcome::Failed(msg)
    }
}

impl Config {
    /// Handle the outcome of a spawned `compact_session`: emit
    /// `CompactingCompleted` on success or `CompactingFailed` (with the full
    /// error cause chain) on failure. Routes through `event_sink` when `Some`,
    /// falling back to the global `emit_agent_event` otherwise — spawned tasks
    /// do not inherit the caller's task-local sink, so the owning turn captures
    /// it before spawning and passes it here.
    pub(crate) fn handle_compaction_result(
        result: &anyhow::Result<()>,
        started: std::time::Instant,
        msg_count: usize,
        event_sink: Option<&std::sync::Arc<dyn harnx_core::event::AgentEventSink>>,
    ) {
        let emit = |event| {
            if let Some(sink) = event_sink {
                sink.emit(event);
            } else {
                harnx_core::sink::emit_agent_event(event);
            }
        };

        match result {
            Ok(()) => {
                log::info!(
                    "compaction: completed in {:?} (messages_before={msg_count})",
                    started.elapsed()
                );
                emit(harnx_core::event::AgentEvent::Session(
                    harnx_core::event::SessionEvent::CompactingCompleted {
                        compaction_id: None,
                        outcome: harnx_core::session::CompactOutcome::Compacted,
                    },
                ));
            }
            Err(err) => {
                warn!(
                    "Failed to compact the session after {:?}: {err}",
                    started.elapsed()
                );
                emit(harnx_core::event::AgentEvent::Session(
                    harnx_core::event::SessionEvent::CompactingFailed {
                        compaction_id: None,
                        error: format!("{err:#}"),
                    },
                ));
            }
        }
    }

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
            harnx_core::event::SessionEvent::CompactingStarted {
                compaction_id: None,
            },
        ));
        let event_sink = harnx_core::sink::current_agent_event_sink();
        tokio::spawn(async move {
            let result =
                Config::with_maintenance_abort(&config, Config::compact_session(&config)).await;
            Self::handle_compaction_result(&result, started, msg_count, event_sink.as_ref());
            if let Some(compacting_session_id) = compacting_session_id.as_deref() {
                if let Some(session) = config.write().session.as_mut() {
                    if session.id == compacting_session_id {
                        session.set_compressing(false);
                    }
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

        {
            let guard = config.read();
            let Some(session) = guard.session.as_ref() else {
                return Ok(());
            };
            if session.id != session_id {
                return Ok(());
            }
        }

        let mut input = harnx_core::input::Input::new(
            transcript.clone(),
            (transcript, vec![]),
            summarizer_agent,
        );
        input.with_session = false;
        input.with_agent = true;

        let summary = crate::config::input::fetch_chat_text(&mut input, config).await?;

        let summary_with_note = append_recovery_note(summary, covered);

        if !Self::apply_compaction_summary(config, &session_id, summary_with_note, split) {
            log::warn!("compaction skipped because the active session changed");
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
/// reader knows detail is recoverable from the NATS log.
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

impl Config {
    pub(crate) async fn with_maintenance_abort<T>(
        config: &GlobalConfig,
        future: impl std::future::Future<Output = Result<T>>,
    ) -> Result<T> {
        let signal = config.read().maintenance_abort.clone();
        let Some(signal) = signal else {
            return future.await;
        };
        tokio::select! {
            biased;
            _ = harnx_core::abort::wait_abort_signal(&signal) => anyhow::bail!("session maintenance cancelled"),
            result = future => result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::event::{AgentEvent, AgentEventSink, SessionEvent};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct CollectingSink {
        events: Mutex<Vec<AgentEvent>>,
    }

    impl AgentEventSink for CollectingSink {
        fn emit(&self, event: AgentEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    #[tokio::test]
    async fn detached_compaction_completion_reaches_captured_scoped_sink() {
        let scoped = Arc::new(CollectingSink::default());

        harnx_core::sink::with_agent_event_sink(scoped.clone(), async {
            let captured = harnx_core::sink::current_agent_event_sink();
            tokio::spawn(async move {
                Config::handle_compaction_result(
                    &Ok(()),
                    std::time::Instant::now(),
                    0,
                    captured.as_ref(),
                );
            })
            .await
            .unwrap();
        })
        .await;

        let events = scoped.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            AgentEvent::Session(SessionEvent::CompactingCompleted { .. })
        ));
    }

    #[tokio::test]
    async fn detached_compaction_failure_reaches_captured_scoped_sink_with_full_error() {
        let scoped = Arc::new(CollectingSink::default());

        harnx_core::sink::with_agent_event_sink(scoped.clone(), async {
            let captured = harnx_core::sink::current_agent_event_sink();
            tokio::spawn(async move {
                let result = Err(anyhow::anyhow!("boom").context("outer"));
                Config::handle_compaction_result(
                    &result,
                    std::time::Instant::now(),
                    0,
                    captured.as_ref(),
                );
            })
            .await
            .unwrap();
        })
        .await;

        let events = scoped.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            AgentEvent::Session(SessionEvent::CompactingFailed { error, .. }) => {
                assert!(error.contains("outer"));
                assert!(error.contains("boom"));
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }
}
