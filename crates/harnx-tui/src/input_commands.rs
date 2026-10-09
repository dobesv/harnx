//! Run dot-commands and render agent/session inspection overlays.

use crate::strip_ansi;
use crate::types::{TranscriptItem, Tui};
use anyhow::{Context, Result};
use harnx_render::pretty_error_string;
use harnx_runtime::config::{
    dump_entries_jsonl, dump_entries_yaml, load_session_for_render, render_metadata_json,
    render_metadata_yaml, SessionFormat, SessionInspectionCommand,
};
use harnx_runtime::nats_session_log::NatsSessionLog;

/// Types of overlay content for info/dump commands.
enum InfoOverlayType {
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

impl Tui {
    pub(crate) async fn submit_dot_command(&mut self, text: String) -> Result<()> {
        let attachments_snapshot = self.app.attachments.clone();
        self.app.transcript.push(TranscriptItem::UserText {
            text: text.clone(),
            seq: None,
            timestamp: Some(chrono::Utc::now()),
        });
        crate::lifecycle::append_markdown_links(&mut self.app.transcript, &text);
        self.render_submitted_attachments(&attachments_snapshot)
            .await;
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
        let (curr_session, curr_agent) = {
            let cfg = self.config.read();
            let s = cfg.session.as_ref().map(|s| s.id().to_string());
            let a = cfg.active_agent_ref();
            (s, a)
        };

        let needs_reconcile = curr_session != prev_session
            || curr_agent != prev_agent
            || [
                ".empty session",
                ".reset session",
                ".reset repl",
                ".compact session",
                ".edit session",
                ".edit message ",
                ".delete message ",
                ".rewind ",
            ]
            .iter()
            .any(|prefix| command_was.starts_with(prefix));

        if !needs_reconcile {
            return;
        }

        self.app.transcript.clear();
        self.app.streaming_open = false;
        self.app.main_streamed_text_idx = None;
        self.app.streamed_text_idx = None;
        // Reset scroll state so the widget doesn't subtract-overflow when
        // the rebuilt transcript is shorter than the previous one.
        self.app.scroll_state = ratatui_widget_scrolling::ScrollState::new();
        self.app.transcript = Self::build_initial_transcript(&self.config).await;
        self.subagent_rows_dirty = true;
        self.pin_transcript_to_bottom();
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
        let result = self.render_info_overlay(&tokens, &info_type).await;
        let display_text = result.unwrap_or_else(|err| format!("Error: {}", err));
        self.open_info_overlay(display_text, info_type);
        true
    }

    fn resolve_info_agent_target(&self, tokens: &[String]) -> anyhow::Result<String> {
        let agent_name = if tokens.len() > 2 {
            tokens[2].clone()
        } else {
            match self.config.read().agent.as_ref() {
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

    async fn render_info_overlay(
        &self,
        tokens: &[String],
        info_type: &InfoOverlayType,
    ) -> Result<String> {
        match info_type {
            InfoOverlayType::AgentInfo => {
                self.resolve_info_agent_target(tokens)
                    .and_then(|agent_name| {
                        let cfg = self.config.read();
                        harnx_runtime::config::render_agent_dump(&cfg, &agent_name)
                    })
            }
            InfoOverlayType::SessionInfo => self.render_info_session_overlay(tokens).await,
            InfoOverlayType::SessionDump => self.render_dump_session_overlay(tokens).await,
        }
    }

    async fn render_info_session_overlay(&self, tokens: &[String]) -> Result<String> {
        let (agent_name, session_id, format) =
            self.resolve_session_target_and_format(tokens, SessionInspectionCommand::Info)?;
        let cfg = self.config.read().clone();
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

    async fn render_dump_session_overlay(&self, tokens: &[String]) -> Result<String> {
        let (agent_name, session_id, format) =
            self.resolve_session_target_and_format(tokens, SessionInspectionCommand::Dump)?;
        let cfg = self.config.read().clone();
        let (jetstream, metadata) =
            harnx_runtime::config::session_metadata_for_agent(&cfg, &agent_name, &session_id)
                .await?;
        let log = NatsSessionLog::new(jetstream, metadata.storage_key());
        let raw = log
            .load_events_async()
            .await
            .with_context(|| format!("Failed to load NATS session '{session_id}'"))?;
        let entries = harnx_core::session_reconstruct::apply_log_mutations_nats(&raw)?;

        match format {
            SessionFormat::Text => {
                let lines = crate::session_overlay::render_transcript_text(&entries);
                Ok(lines)
            }
            SessionFormat::Yaml => dump_entries_yaml(entries.iter().map(|(_, e)| e)),
            SessionFormat::Json => dump_entries_jsonl(entries.iter().map(|(_, e)| e)),
        }
    }

    fn resolve_session_target_and_format(
        &self,
        tokens: &[String],
        command: SessionInspectionCommand,
    ) -> Result<(String, String, SessionFormat)> {
        harnx_runtime::config::parse_session_inspection_args(&tokens[2..], command)
    }

    fn open_info_overlay(&mut self, text: String, info_type: InfoOverlayType) {
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
        if harnx_runtime::operator_tools::is_operator_tool_command(line) {
            match harnx_runtime::operator_tools::parse_operator_line(line) {
                Ok(command) => self.start_operator_tool_command(command),
                Err(error) => self
                    .app
                    .transcript
                    .push(TranscriptItem::ErrorText(pretty_error_string(&error))),
            }
            return Ok(());
        }
        if self.try_handle_info_overlay(line.trim_start()).await {
            return Ok(());
        }
        let prev_session = self
            .config
            .read()
            .session
            .as_ref()
            .map(|s| s.id().to_string());
        let prev_agent = self.config.read().active_agent_ref();
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

        let clean = strip_ansi(&captured).trim_end_matches('\n').to_string();
        let line_cmd = line.trim_start();
        let is_mutation_command = line_cmd.starts_with(".edit message ")
            || line_cmd.starts_with(".delete message ")
            || line_cmd.starts_with(".rewind ");

        self.finish_command(
            result,
            clean,
            (line, prev_session, prev_agent, is_mutation_command),
        )
        .await;

        Ok(())
    }

    async fn finish_command(
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
                    self.pin_transcript_to_bottom();
                }
            }
            Err(err) => {
                self.app
                    .transcript
                    .push(TranscriptItem::ErrorText(pretty_error_string(&err)));
            }
        }
    }
}
