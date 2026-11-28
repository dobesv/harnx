//! Run dot-commands and render agent/session inspection overlays.

use crate::history_work::{
    invalidates_transcript_coordinates, is_mutation_command, is_session_bound_history_command,
};
use crate::strip_ansi;
use crate::types::{TranscriptItem, Tui};
use anyhow::{Context, Result};
use harnx_render::pretty_error_string;
use harnx_runtime::config::{
    dump_entries_jsonl, dump_entries_yaml, load_session_for_render, render_metadata_json,
    render_metadata_yaml, GlobalConfig, SessionFormat, SessionInspectionCommand,
};
use harnx_runtime::nats_session_log::NatsSessionLog;

/// Types of overlay content for info/dump commands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InfoOverlayType {
    AgentInfo,
    SessionInfo,
    SessionDump,
}

impl InfoOverlayType {
    fn title(&self) -> &'static str {
        match self {
            InfoOverlayType::AgentInfo => "Agent Info",
            InfoOverlayType::SessionInfo => "Session Info",
            InfoOverlayType::SessionDump => "Session Transcript",
        }
    }
}

/// Commands that can wait on managed-local acquisition or the session log
/// before `finish_command` reconciles the transcript. They run as
/// cancellable background work off the input loop (design 9).
fn is_history_command(line: &str) -> bool {
    let line_cmd = line.trim_start();
    is_session_bound_history_command(line_cmd)
        || invalidates_transcript_coordinates(line_cmd)
        || [".session", "/session"]
            .iter()
            .any(|prefix| line_cmd.starts_with(prefix))
}

impl Tui {
    pub(crate) async fn submit_dot_command(&mut self, text: String) -> Result<()> {
        if self.reject_busy_title_rename(&text) {
            return Ok(());
        }
        self.commit_submitted_dot_command(text).await
    }

    fn reject_busy_title_rename(&mut self, text: &str) -> bool {
        let busy = harnx_runtime::config::Config::parse_dot_set_title(text).is_some()
            && self.is_title_rename_busy();
        if busy {
            self.show_title_rename_busy_notice();
        }
        busy
    }

    async fn commit_submitted_dot_command(&mut self, text: String) -> Result<()> {
        let attachments_snapshot = self.app.attachments.clone();
        self.app.transcript.push(TranscriptItem::UserText {
            text: text.clone(),
            seq: None,
            timestamp: Some(chrono::Utc::now()),
        });
        crate::lifecycle::append_markdown_links(&mut self.app.transcript, &text);
        self.render_submitted_attachments(&attachments_snapshot)
            .await;
        self.note_live_transcript_mutation();
        self.pin_transcript_to_bottom();
        self.app.input = Self::new_input();
        self.run_command(&text).await?;
        self.refresh_input_chrome();
        Ok(())
    }

    pub(crate) async fn reconcile_transcript_after_command(
        &mut self,
        prev_session: Option<String>,
        prev_agent: Option<String>,
        command_was: &str,
    ) {
        // Shared session-switch reconciliation. Clears stale focus, selection,
        // streaming state, detail/document overlays, and rebuilds the transcript.
        // Called after .session navigation, handoff Enter, rewind/delete edits,
        // and agent/session switching via input_modal.rs or lifecycle.rs.
        let (curr_session, curr_agent) = {
            let cfg = self.config.read();
            let s = cfg.session.as_ref().map(|s| s.id().to_string());
            let a = cfg.active_agent_ref();
            (s, a)
        };

        let current_dest = self.current_history_destination();
        let binding_switched = self
            .transcript_view_ownership
            .active_binding
            .as_ref()
            .is_some_and(|b| {
                b.target != current_dest.target || b.storage_key != current_dest.storage_key
            });
        let destination_switched =
            self.transcript_view_ownership.active_destination != current_dest;
        let target_switched = curr_session != prev_session
            || curr_agent != prev_agent
            || binding_switched
            || destination_switched;
        let invalidates_coordinates = invalidates_transcript_coordinates(command_was);
        let needs_reconcile =
            target_switched || invalidates_coordinates || command_was.starts_with(".edit session");

        self.transcript_view_ownership.active_destination = current_dest;
        if !needs_reconcile {
            return;
        }

        if target_switched {
            self.retire_prior_target_transcript_actions();
            self.sync_session_activity_monitor();
        }
        self.schedule_history_rebuild_for_command(target_switched);
    }

    async fn try_handle_info_overlay(&mut self, line_cmd: &str) -> bool {
        let info_type = self.detect_info_overlay_type(line_cmd);
        if info_type.is_none() {
            return false;
        }

        let Ok(tokens) = shell_words::split(line_cmd) else {
            self.app.transcript.push(TranscriptItem::ErrorText(
                "Unclosed quotes in command".to_string(),
            ));
            return true;
        };

        let info_type = info_type.unwrap();
        // Session info/dump loads read the session log and can wait on
        // managed-local acquisition, so they run as cancellable background
        // work and land here as `InfoOverlayLoaded` (design 9). The input
        // loop stays responsive while a slow broker recovers.
        self.app.transcript.push(TranscriptItem::StatusLine(
            "Loading session info…".to_string(),
        ));
        self.pin_transcript_to_bottom();
        self.spawn_info_overlay_load(tokens, info_type);
        true
    }

    fn resolve_info_agent_target(
        config: &GlobalConfig,
        tokens: &[String],
    ) -> anyhow::Result<String> {
        let agent_name = if tokens.len() > 2 {
            tokens[2].clone()
        } else {
            match config.read().agent.as_ref() {
                Some(a) => a.name().to_string(),
                None => String::new(),
            }
        };

        if agent_name.is_empty() {
            Err(anyhow::anyhow!(
                "No active agent and no agent name provided. Usage: .info agent [<name>]"
            ))
        } else {
            Ok(agent_name)
        }
    }

    fn detect_info_overlay_type(&self, line_cmd: &str) -> Option<InfoOverlayType> {
        if line_cmd.starts_with(".info agent") || line_cmd.starts_with("/info agent") {
            Some(InfoOverlayType::AgentInfo)
        } else if line_cmd.starts_with(".info session") || line_cmd.starts_with("/info session") {
            Some(InfoOverlayType::SessionInfo)
        } else if line_cmd.starts_with(".dump session") || line_cmd.starts_with("/dump session") {
            Some(InfoOverlayType::SessionDump)
        } else {
            None
        }
    }

    /// Render the info/dump overlay content. Associated, not `&self`, so the
    /// background job in `history_work.rs` can run it detached from the TUI.
    pub(crate) async fn render_info_overlay(
        config: &GlobalConfig,
        tokens: &[String],
        info_type: &InfoOverlayType,
    ) -> Result<String> {
        match info_type {
            InfoOverlayType::AgentInfo => {
                Self::resolve_info_agent_target(config, tokens).and_then(|agent_name| {
                    let cfg = config.read();
                    harnx_runtime::config::render_agent_dump(&cfg, &agent_name)
                })
            }
            InfoOverlayType::SessionInfo => Self::render_info_session_overlay(config, tokens).await,
            InfoOverlayType::SessionDump => Self::render_dump_session_overlay(config, tokens).await,
        }
    }

    // qual:allow(iosp) reason: "mirrors the CLI inspection command's format dispatch in one place; per-format helpers would scatter the format contract"
    async fn render_info_session_overlay(
        config: &GlobalConfig,
        tokens: &[String],
    ) -> Result<String> {
        let (agent_name, session_id, format) =
            Self::resolve_session_target_and_format(tokens, SessionInspectionCommand::Info)?;
        let cfg = config.read().clone();
        let (agent, cluster) = cfg.resolve_session_agent(&agent_name)?;
        match format {
            SessionFormat::Text => {
                let session =
                    load_session_for_render(&cfg, Some(&cluster), &session_id, &agent).await?;
                harnx_runtime::config::session::render(&session)
            }
            SessionFormat::Yaml | SessionFormat::Json => {
                let (_, metadata) = harnx_runtime::config::session_metadata_for_agent(
                    &cfg,
                    &agent_name,
                    &session_id,
                )
                .await?;
                match format {
                    SessionFormat::Yaml => render_metadata_yaml(&metadata),
                    SessionFormat::Json => render_metadata_json(&metadata),
                    SessionFormat::Text => unreachable!(),
                }
            }
        }
    }

    // qual:allow(iosp) reason: "mirrors the CLI dump command's format dispatch in one place; per-format helpers would scatter the format contract"
    async fn render_dump_session_overlay(
        config: &GlobalConfig,
        tokens: &[String],
    ) -> Result<String> {
        let (agent_name, session_id, format) =
            Self::resolve_session_target_and_format(tokens, SessionInspectionCommand::Dump)?;
        let cfg = config.read().clone();
        let (jetstream, metadata) =
            harnx_runtime::config::session_metadata_for_agent(&cfg, &agent_name, &session_id)
                .await?;
        let log = NatsSessionLog::new(jetstream, metadata.storage_key());
        let raw = log
            .load_events_async()
            .await
            .with_context(|| format!("Failed to load NATS session '{session_id}'"))?;
        if matches!(format, SessionFormat::Text) {
            return crate::session_overlay::render_transcript_text(&raw);
        }
        let entries = harnx_core::session_reconstruct::apply_log_mutations_nats(&raw)?;
        match format {
            SessionFormat::Text => unreachable!(),
            SessionFormat::Yaml => dump_entries_yaml(entries.iter().map(|(_, e)| e)),
            SessionFormat::Json => dump_entries_jsonl(entries.iter().map(|(_, e)| e)),
        }
    }

    fn resolve_session_target_and_format(
        tokens: &[String],
        command: SessionInspectionCommand,
    ) -> Result<(String, String, SessionFormat)> {
        harnx_runtime::config::parse_session_inspection_args(&tokens[2..], command)
    }

    // `pub(crate)`: the background-work completion event in event_isolation.rs
    // opens the overlay once the (possibly broker-waiting) load finished.
    pub(crate) fn open_info_overlay(&mut self, text: String, info_type: InfoOverlayType) {
        self.app.detail_view_scroll = {
            let mut s = ratatui_widget_scrolling::ScrollState::new();
            s.follow = false;
            s
        };
        self.app.doc_view = None;
        self.app.doc_history.clear();
        self.app.detail_view_text = Some(text);
        self.app.detail_view_entry = None;
        self.app.detail_view_title = Some(info_type.title().to_string());
        self.app.detail_view_open = true;
    }

    pub(super) async fn run_command(&mut self, line: &str) -> Result<()> {
        if self.reject_stale_mutation_command(line) || self.try_handle_special_command(line).await {
            return Ok(());
        }
        self.run_config_command(line).await
    }

    fn reject_stale_mutation_command(&mut self, line: &str) -> bool {
        let stale = is_mutation_command(line) && self.actionable_transcript_binding().is_none();
        if stale {
            self.show_stale_transcript_notice();
        }
        stale
    }

    async fn try_handle_special_command(&mut self, line: &str) -> bool {
        #[cfg(test)]
        if let Some(record) = self.command_override.clone() {
            // Deterministic test seam: observe the dispatched command without
            // touching any session state.
            record(line);
            return true;
        }
        if let Some(action) = crate::credentials::parse_credentials_dot_command(line) {
            match action {
                Ok(action) => self.start_credentials_command(action),
                Err(error) => self.report_command_error(&error),
            }
            return true;
        }
        if harnx_runtime::operator_tools::is_operator_tool_command(line) {
            match harnx_runtime::operator_tools::parse_operator_line(line) {
                Ok(command) => self.start_operator_tool_command(command),
                Err(error) => self.report_command_error(&error),
            }
            return true;
        }
        self.try_handle_info_overlay(line.trim_start()).await
    }

    pub(super) async fn run_bound_history_command(
        &mut self,
        line: &str,
        expected_target: crate::types::TranscriptActionTarget,
    ) -> Result<()> {
        if !self.validate_history_command_target(Some(&expected_target)) {
            self.show_stale_transcript_notice();
            return Ok(());
        }
        #[cfg(test)]
        if let Some(record) = self.command_override.clone() {
            // Deterministic test seam: observe the dispatched command without
            // touching any session state.
            record(line);
            return Ok(());
        }
        self.enqueue_bound_history_command(line.to_string(), Some(expected_target));
        Ok(())
    }

    /// A failed command parse is reported like any other command output.
    fn report_command_error(&mut self, error: &anyhow::Error) {
        self.app
            .transcript
            .push(TranscriptItem::ErrorText(pretty_error_string(error)));
        self.note_live_transcript_mutation();
    }

    /// Run the line through the config command runner, then finish it with
    /// the transcript and UI updates `finish_command` owns.
    async fn run_config_command(&mut self, line: &str) -> Result<()> {
        // History commands (session switches, transcript mutations) can wait
        // on managed-local acquisition or the session log. `.set title` runs
        // as a single-flight submission-bound rename off the input loop so a
        // recovering broker never stalls keys or Ctrl-C. Other commands stay
        // inline: they never wait on the broker.
        if let Some(raw_title) = harnx_runtime::config::Config::parse_dot_set_title(line) {
            self.submit_title_rename(raw_title);
            return Ok(());
        }
        self.run_non_title_config_command(line).await
    }

    async fn run_non_title_config_command(&mut self, line: &str) -> Result<()> {
        if is_history_command(line) {
            self.enqueue_config_history_command(line);
            return Ok(());
        }
        self.run_config_command_inline(line).await
    }

    fn submit_title_rename(&mut self, raw_title: &str) {
        match harnx_runtime::config::Config::capture_manual_title_write(&self.config, raw_title) {
            Ok((target, record)) => self.spawn_title_rename(target, record),
            Err(err) => self.report_command_error(&err),
        }
    }

    fn enqueue_config_history_command(&mut self, line: &str) {
        if is_mutation_command(line) {
            let Some(binding) = self.actionable_transcript_binding() else {
                self.show_stale_transcript_notice();
                return;
            };
            self.enqueue_bound_history_command(line.to_string(), Some(binding));
        } else if is_session_bound_history_command(line) {
            let session_target = self.current_session_command_target();
            self.enqueue_session_history_command(line.to_string(), session_target);
        } else {
            self.enqueue_history_command(line.to_string());
        }
    }

    fn retire_title_rename_if_target_changed(
        &mut self,
        prev_target: &crate::types::HistoryTarget,
        prev_storage_key: &Option<String>,
    ) {
        let (curr_target, curr_storage_key) = self.current_history_target_and_storage_key();
        if &curr_target != prev_target || &curr_storage_key != prev_storage_key {
            self.retire_pending_title_rename();
        }
    }

    pub(crate) async fn run_config_command_inline(&mut self, line: &str) -> Result<()> {
        let (prev_history_target, prev_storage_key) = self.current_history_target_and_storage_key();
        let prev_session = prev_history_target.session_id.clone();
        let prev_agent = prev_history_target.agent.clone();
        // Run the command inside a block that owns the lock guards so they are
        // dropped before we touch `self` again for transcript / UI updates.
        let (result, captured) = {
            let config = self.config.clone();
            let abort_signal = self.abort_signal.clone();
            let mut output = Vec::<u8>::new();

            // Command futures carry agent/session initialization state. Keep
            // that frame off the nested TUI event-dispatch stack.
            let result = Box::pin(
                harnx_runtime::commands::run_command_with_output_and_local_worker(
                    &config,
                    abort_signal,
                    line,
                    &mut output,
                    &self.local_worker,
                ),
            )
            .await;

            let captured = String::from_utf8_lossy(&output).into_owned();
            (result, captured)
        };

        self.retire_title_rename_if_target_changed(&prev_history_target, &prev_storage_key);
        let clean = strip_ansi(&captured).trim_end_matches('\n').to_string();
        self.finish_command(
            result,
            clean,
            (line, prev_session, prev_agent, is_mutation_command(line)),
        )
        .await;

        Ok(())
    }

    pub(crate) async fn finish_command(
        &mut self,
        result: Result<harnx_runtime::commands::CommandOutcome>,
        clean: String,
        ctx: (&str, Option<String>, Option<String>, bool),
    ) {
        let (line, prev_session, prev_agent, is_mutation_command) = ctx;
        match result {
            Ok(outcome) => {
                self.maybe_open_picker_after_command(outcome, prev_agent.clone())
                    .await;
                // Sync terminal status emission with current config after `.set`.
                // Commands like `.set terminal_status true/false` update config on success.
                crate::terminal_status::set_enabled(self.config.read().terminal_status);
                let llm_busy = self.app.llm_busy;
                let pending_message = self.app.pending_message.is_some();
                Self::refresh_input_chrome_from_state(
                    &self.config,
                    &mut self.app,
                    llm_busy,
                    pending_message,
                );
                self.reconcile_transcript_after_command(prev_session, prev_agent, line)
                    .await;
                if !clean.is_empty() {
                    if is_mutation_command {
                        self.app
                            .transcript
                            .push(TranscriptItem::MutationNotice(clean.clone()));
                    } else {
                        self.app
                            .transcript
                            .push(TranscriptItem::SystemText(clean.clone()));
                    }
                    // A command notice is preserved in the reload's tail.
                    if !invalidates_transcript_coordinates(line) {
                        self.note_live_transcript_mutation();
                    }
                    self.pin_transcript_to_bottom();
                }
            }
            Err(err) => {
                let needs_reload = invalidates_transcript_coordinates(line);
                if needs_reload {
                    self.reconcile_transcript_after_command(prev_session, prev_agent, line)
                        .await;
                }
                self.app
                    .transcript
                    .push(TranscriptItem::ErrorText(pretty_error_string(&err)));
                if !needs_reload {
                    self.note_live_transcript_mutation();
                }
            }
        }
    }
}
