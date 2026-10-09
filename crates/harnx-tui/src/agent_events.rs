//! Project agent events, streaming replies and compaction outcomes into the transcript.

use crate::lifecycle::session_history_transcript_items;
use crate::strip_ansi;
use crate::tool_transcript::{
    blocked_tool_to_transcript_items, tool_completed_to_transcript_items,
    tool_started_to_transcript_items,
};
use crate::types::{TranscriptItem, Tui};
use crossterm::ExecutableCommand;
use harnx_core::event::{AgentEvent, AgentSource, SessionEvent};

/// Concatenate `ContentBlock::Text(..)` fragments into a single String.
/// Non-Text blocks (Image, ResourceLink, Opaque) are skipped — the TUI
/// transcript currently only renders text.
pub(super) fn concat_text_blocks(blocks: &[harnx_core::event::ContentBlock]) -> String {
    use harnx_core::event::ContentBlock;
    let mut out = String::new();
    for block in blocks {
        if let ContentBlock::Text(t) = block {
            out.push_str(t);
        }
    }
    out
}

/// Normalize a streamed thought fragment before adding it to a transcript.
/// Keep whitespace-only fragments because they preserve line breaks between
/// adjacent streamed chunks.
pub(super) fn clean_thought_chunk(text: &str) -> String {
    strip_ansi(text)
        .trim_start_matches("<think>")
        .trim_end_matches("</think>")
        .to_string()
}

/// Reproduce the textual representation of `CompletionTokenUsage` that the
/// legacy `UiOutputEventKind::LlmFinal { usage: CompletionTokenUsage }` path
/// produced. Pre-migration the TUI tested `!usage.is_empty()` (input==0 &&
/// output==0) and then formatted via `format!("Usage: {usage}")` using the
/// Display impl. Mirror that contract: return empty when `is_empty()`, else
/// the Display output. Callers then test `!usage_str.is_empty()` to decide
/// whether to emit a `Usage:` transcript line.
fn format_usage(usage: &harnx_core::api_types::CompletionTokenUsage) -> String {
    if usage.is_empty() {
        String::new()
    } else {
        format!("{usage}")
    }
}

fn notice_items(event: harnx_core::event::NoticeEvent) -> Vec<TranscriptItem> {
    use harnx_core::event::NoticeEvent;
    let text = match event {
        NoticeEvent::Info(text) => text,
        NoticeEvent::Warning(text) => format!("⚠ {text}"),
        NoticeEvent::Error(text) => format!("error: {text}"),
    };
    let clean = strip_ansi(&text).trim_end_matches('\n').to_string();
    if clean.is_empty() {
        vec![]
    } else {
        vec![TranscriptItem::SystemText(clean)]
    }
}

fn replayed_user_items(content: String) -> Vec<TranscriptItem> {
    // Replayed history must not consume a later live LogSeqAssigned event.
    let mut items = vec![TranscriptItem::UserText {
        text: content.clone(),
        seq: None,
        timestamp: None,
    }];
    crate::lifecycle::append_markdown_links(&mut items, &content);
    items
}

impl Tui {
    pub(super) fn flush_pending_thought(&mut self) {
        if self.app.pending_thought_text.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.app.pending_thought_text);
        self.app.pending_thought_source = None;
        self.app
            .transcript
            .push(TranscriptItem::ThoughtText(text.trim().to_string()));
    }

    #[cfg(test)]
    pub(crate) fn flush_pending_thought_for_test(&mut self) {
        self.flush_pending_thought();
    }

    async fn render_compaction_event(&mut self, event: SessionEvent) -> Vec<TranscriptItem> {
        match event {
            SessionEvent::CompactingStarted { .. } => vec![TranscriptItem::SystemText(
                "Compacting session…".to_string(),
            )],
            SessionEvent::CompactingCompleted { outcome, .. } => match outcome {
                harnx_core::session::CompactOutcome::Compacted => {
                    // Normal completion - reload transcript
                    self.app.transcript = session_history_transcript_items(&self.config).await;
                    self.subagent_rows_dirty = true;
                    self.app.streaming_open = false;
                    // A compaction can land mid-turn after some assistant text has
                    // already streamed. The rebuild drops the parent streamed row, so its
                    // replacement index must also be cleared before the eventual Final event.
                    self.app.main_streamed_text_idx = None;
                    self.app.streamed_text_idx = None;
                    // The rebuild drops all SourceHeading entries, so the next
                    // output must re-emit its heading even if it shares the prior
                    // source. Without this, the first post-compaction message would
                    // render without an agent label.
                    self.app.last_ui_output_source = None;
                    // The transcript is entirely rebuilt, so any prior focus/anchor
                    // indices reference now-different items even when still in
                    // bounds. Clear selection/detail state unconditionally.
                    self.app.transcript_focus = None;
                    self.app.transcript_selection_anchor = None;
                    self.pin_transcript_to_bottom();
                    vec![]
                }
                harnx_core::session::CompactOutcome::Unchanged(reason) => {
                    // Nothing was compacted - neutral system message, NOT an error.
                    // Clear any spinner state (CompactingStarted may have shown one).
                    let text = match reason {
                        harnx_core::session::UnchangedReason::NoUserMessages => {
                            "No user messages to compact"
                        }
                        harnx_core::session::UnchangedReason::NothingEligible => {
                            "Nothing eligible for compaction"
                        }
                        harnx_core::session::UnchangedReason::AlreadyCompacted => {
                            "Session already compacted"
                        }
                    };
                    vec![TranscriptItem::SystemText(text.to_string())]
                }
                harnx_core::session::CompactOutcome::Failed(err) => {
                    // Treat as error - this shouldn't normally come through Completed,
                    // but handle it defensively.
                    vec![TranscriptItem::ErrorText(format!(
                        "Compaction failed: {err}"
                    ))]
                }
            },
            SessionEvent::CompactingFailed { error, .. } => vec![TranscriptItem::ErrorText(
                format!("Compaction failed: {error}"),
            )],
            _ => vec![],
        }
    }

    pub(super) async fn render_agent_event(&mut self, event: AgentEvent) {
        use harnx_core::event::ModelEvent;
        let (source, event, is_sub_agent) = match event {
            AgentEvent::SubAgent { source, event } => (Some(source), *event, true),
            event => (None, event, false),
        };
        self.prepare_assistant_boundary(&event, source.as_ref(), is_sub_agent);
        if self.handle_session_event(&event, is_sub_agent).await {
            return;
        }
        // Sequence assignment is bookkeeping, not output: don't flush thoughts
        // or create a source heading before binding it to a live row.
        if let AgentEvent::Session(SessionEvent::LogSeqAssigned { seq }) = event {
            self.assign_live_log_seq(seq);
            return;
        }
        if self.render_model_source_change(&event) {
            return;
        }
        let is_thought = matches!(&event, AgentEvent::Model(ModelEvent::ThoughtChunk { .. }));
        if !is_thought {
            self.flush_pending_thought();
        }
        self.render_ui_output_heading(source.as_ref());
        let entries = self
            .project_agent_event(event, source.as_ref(), is_sub_agent)
            .await;
        let has_entries = !entries.is_empty();
        self.app.transcript.extend(entries);
        if has_entries || is_thought {
            self.pin_transcript_to_bottom();
        }
    }

    fn assign_live_log_seq(&mut self, seq: usize) {
        // Try to backfill the seq into the most recent unsequenced transcript
        // item (UserText, AssistantText, or ToolCall). If an item is found and
        // patched, the seq has been consumed — clear pending_tool_seq.  If no
        // item is found yet (e.g. ToolEvent::Started arrives after this event),
        // store seq in pending_tool_seq so the upcoming ToolCall can pick it up.
        let mut backfilled = false;
        for item in self.app.transcript.iter_mut().rev() {
            match item {
                // Only backfill "live" entries — items with a timestamp are
                // created during an active session.  The agent banner is
                // AssistantText { seq: None, timestamp: None } and must not
                // consume a seq that belongs to the first real message.
                TranscriptItem::UserText {
                    seq: item_seq @ None,
                    timestamp: Some(_),
                    ..
                }
                | TranscriptItem::AssistantText {
                    seq: item_seq @ None,
                    timestamp: Some(_),
                    ..
                }
                | TranscriptItem::ToolCall {
                    seq: item_seq @ None,
                    timestamp: Some(_),
                    ..
                } => {
                    *item_seq = Some(seq);
                    backfilled = true;
                    break;
                }
                _ => {}
            }
        }
        if backfilled {
            // Seq consumed by an existing item; clear any pending slot.
            self.app.pending_tool_seq = None;
        } else {
            // No existing item to patch; save for the next ToolCall creation.
            self.app.pending_tool_seq = Some(seq);
        }
    }

    async fn project_agent_event(
        &mut self,
        event: AgentEvent,
        source: Option<&AgentSource>,
        is_sub_agent: bool,
    ) -> Vec<TranscriptItem> {
        match event {
            AgentEvent::Notice(notice) => notice_items(notice),
            AgentEvent::User(harnx_core::event::UserEvent::Message { content }) => {
                replayed_user_items(content)
            }
            AgentEvent::Tool(event) => self.project_tool_event(event),
            AgentEvent::Model(event) => {
                self.project_model_event(event, source, is_sub_agent).await;
                vec![]
            }
            AgentEvent::Plan { entries } => vec![TranscriptItem::Plan(entries)],
            AgentEvent::Session(event) => self.project_session_event(event).await,
            AgentEvent::SubAgent { .. } => unreachable!("sub-agent event flattened above"),
            _ => vec![],
        }
    }

    fn project_tool_event(&mut self, event: harnx_core::event::ToolEvent) -> Vec<TranscriptItem> {
        use harnx_core::event::ToolEvent;
        match event {
            ToolEvent::Progress { .. } => vec![],
            ToolEvent::Completed {
                id,
                output,
                markdown,
                ..
            } => {
                crate::tool_render::complete_tool_call(&mut self.app.transcript, &id);
                tool_completed_to_transcript_items(&output, markdown.as_deref())
            }
            ToolEvent::Failed { id, error } => {
                crate::tool_render::fail_tool_call(&mut self.app.transcript, &id);
                vec![TranscriptItem::ErrorText(format!("tool failed: {error}"))]
            }
            ToolEvent::Update {
                id,
                markdown,
                status,
                title,
                kind,
                locations,
                usage,
                ..
            } => {
                crate::tool_render::apply_tool_event_update(
                    &mut self.app.transcript,
                    crate::tool_render::ToolUpdatePayload {
                        id,
                        markdown,
                        status,
                        title,
                        kind,
                        locations,
                        usage,
                    },
                    self.app.pending_tool_seq,
                );
                vec![]
            }
            event @ ToolEvent::Started { .. } => {
                tool_started_to_transcript_items(event, self.app.pending_tool_seq)
            }
            event @ ToolEvent::Blocked { .. } => {
                blocked_tool_to_transcript_items(event, self.app.pending_tool_seq)
            }
        }
    }

    async fn project_model_event(
        &mut self,
        event: harnx_core::event::ModelEvent,
        source: Option<&AgentSource>,
        is_sub_agent: bool,
    ) {
        use harnx_core::event::ModelEvent;
        match event {
            ModelEvent::MessageChunk { blocks } => {
                self.append_streaming_assistant_chunk(&concat_text_blocks(&blocks), is_sub_agent);
            }
            ModelEvent::ThoughtChunk { blocks } => {
                self.append_pending_thought_chunk(&concat_text_blocks(&blocks), source);
            }
            ModelEvent::Final { output, usage } if is_sub_agent => {
                self.render_sub_agent_final(output, &usage)
            }
            ModelEvent::Final { output, usage } => {
                self.finish_main_prompt_final(output, &usage).await
            }
            ModelEvent::Error(error) if is_sub_agent => self.render_sub_agent_error(error),
            ModelEvent::Error(error) => self.finish_main_prompt_error(error).await,
            _ => {}
        }
    }

    fn append_pending_thought_chunk(&mut self, text: &str, source: Option<&AgentSource>) {
        let clean = clean_thought_chunk(text);
        // Whitespace is significant between streamed thought fragments.
        if clean.is_empty() {
            return;
        }
        if self.app.pending_thought_source.as_ref() != source {
            self.flush_pending_thought();
            self.app.pending_thought_source = source.cloned();
        }
        self.app.pending_thought_text.push_str(&clean);
    }

    async fn project_session_event(&mut self, event: SessionEvent) -> Vec<TranscriptItem> {
        match event {
            event @ (SessionEvent::CompactingStarted { .. }
            | SessionEvent::CompactingCompleted { .. }
            | SessionEvent::CompactingFailed { .. }) => self.render_compaction_event(event).await,
            SessionEvent::TitleGenerationFailed(error) => vec![TranscriptItem::ErrorText(format!(
                "Title generation failed: {error}"
            ))],
            SessionEvent::TitleUpdated(title) => {
                let _ = std::io::stdout().execute(crossterm::terminal::SetTitle(&title));
                vec![]
            }
            SessionEvent::Generic { text } => vec![TranscriptItem::SystemText(text)],
            _ => vec![],
        }
    }

    async fn finish_main_prompt_final(
        &mut self,
        output: String,
        usage: &harnx_core::api_types::CompletionTokenUsage,
    ) {
        self.flush_pending_thought();
        let usage_str = format_usage(usage);
        if !output.is_empty() {
            self.finish_main_reply(output);
        } else {
            self.close_assistant_stream();
        }
        self.app.streaming_open = false;
        self.app.main_streamed_text_idx = None;
        self.app.streamed_text_idx = None;
        if !usage_str.is_empty() {
            self.app
                .transcript
                .push(TranscriptItem::SystemText(format!("Usage: {usage_str}")));
            self.pin_transcript_to_bottom();
        }
        self.refresh_input_chrome();
    }

    fn finish_main_reply(&mut self, output: String) {
        let index = crate::assistant_transcript::replace_or_append_assistant(
            &mut self.app.transcript,
            self.app.main_streamed_text_idx,
            output,
        );
        self.app.main_streamed_text_idx = Some(index);
        self.refresh_root_assistant_links(index);
        self.pin_transcript_to_bottom();
    }

    fn prepare_assistant_boundary(
        &mut self,
        event: &AgentEvent,
        source: Option<&AgentSource>,
        is_sub_agent: bool,
    ) {
        if matches!(
            event,
            AgentEvent::Session(SessionEvent::LogSeqAssigned { .. })
        ) {
            return;
        }
        let source_changed = source != self.app.last_ui_output_source.as_ref();
        if source_changed || crate::assistant_transcript::closes_assistant_stream(event) {
            self.close_assistant_stream();
        }
        if source_changed || crate::assistant_transcript::ends_assistant_round(event) {
            self.app.streamed_text_idx = None;
        }
        // A tool call ends this model response. A child/source or thought
        // transition only closes aggregation; the parent Final can still follow.
        if !is_sub_agent && crate::assistant_transcript::ends_assistant_round(event) {
            self.app.main_streamed_text_idx = None;
        }
    }

    pub(super) fn close_assistant_stream(&mut self) {
        if self.app.streaming_open {
            if let Some(index) = self.app.streamed_text_idx {
                self.refresh_root_assistant_links(index);
            }
            self.app.streaming_open = false;
        }
    }

    fn refresh_root_assistant_links(&mut self, index: usize) {
        crate::assistant_transcript::refresh_assistant_links(
            &mut self.app.transcript,
            index,
            [
                &mut self.app.transcript_focus,
                &mut self.app.transcript_selection_anchor,
                &mut self.app.main_streamed_text_idx,
                &mut self.app.streamed_text_idx,
            ],
        );
    }

    pub(super) async fn finish_main_prompt_error(&mut self, err: String) {
        self.close_assistant_stream();
        self.flush_pending_thought();
        // Emit terminal status: model error.
        crate::terminal_status::set_status(crate::terminal_status::TerminalStatus::Error);
        self.app.streaming_open = false;
        self.app.main_streamed_text_idx = None;
        self.app.streamed_text_idx = None;
        self.app.last_ui_output_source = None;
        self.app.transcript.push(TranscriptItem::ErrorText(err));
        if self
            .current_prompt_abort
            .as_ref()
            .is_some_and(|abort| abort.aborted())
        {
            // The user may type a new message while the cancelled task winds
            // down. Keep that newer message queued; Turn::Ended (or the task
            // fallback) will submit it after the old task actually exits.
            self.pin_transcript_to_bottom();
            self.refresh_input_chrome();
            return;
        }
        *self.shared_pending_message.lock().await = None;

        // Do not replay input that failed. Restore it as editable draft so user
        // can change it before resubmitting, avoiding persistent retry loops.
        if let Some(pending) = self.app.pending_message.take() {
            self.set_input_text(&pending.text);
            self.app.attachments = pending.attachments;
            self.app.attachment_dir = pending.attachment_dir;
            self.app.paste_count = pending.paste_count;
            self.app.transcript.push(TranscriptItem::SystemText(
                "Queued message not sent due to error. Press Enter to retry.".to_string(),
            ));
        }
        self.pin_transcript_to_bottom();
        self.refresh_input_chrome();
    }

    /// Render a nested sub-agent's final turn text under its source heading.
    /// Deliberately touches no main-task state (busy flag, abort signal,
    /// pending message): the tool call that delegated to the sub-agent is
    /// still in flight, so the main prompt task is not done.
    fn render_sub_agent_final(
        &mut self,
        output: String,
        usage: &harnx_core::api_types::CompletionTokenUsage,
    ) {
        if !output.is_empty() {
            let index = crate::assistant_transcript::replace_or_append_assistant(
                &mut self.app.transcript,
                self.app.streamed_text_idx,
                output,
            );
            self.refresh_root_assistant_links(index);
        } else {
            self.close_assistant_stream();
        }
        // Close any open streaming run so a later chunk from the same source
        // starts a fresh block instead of appending to this final text.
        self.app.streaming_open = false;
        self.app.streamed_text_idx = None;
        let usage_str = format_usage(usage);
        if !usage_str.is_empty() {
            self.app
                .transcript
                .push(TranscriptItem::SystemText(format!("Usage: {usage_str}")));
        }
        self.pin_transcript_to_bottom();
    }

    /// Render a nested sub-agent's error. Like `render_sub_agent_final`,
    /// this only surfaces the text — the main prompt task keeps running,
    /// so busy state and the queued pending message stay untouched.
    fn render_sub_agent_error(&mut self, err: String) {
        self.app.transcript.push(TranscriptItem::ErrorText(err));
        self.app.streaming_open = false;
        self.app.streamed_text_idx = None;
        self.pin_transcript_to_bottom();
    }

    pub(super) fn render_ui_output_heading(&mut self, source: Option<&AgentSource>) {
        let source = source.cloned();
        if source != self.app.last_ui_output_source {
            if let Some(source) = &source {
                self.app
                    .transcript
                    .push(TranscriptItem::SourceHeading(source.clone()));
            }
            self.app.last_ui_output_source = source;
            // Reset streaming-assistant tracking: a source change means the
            // next MessageChunk event belongs to a different agent than
            // whatever the previous AssistantText entry was aggregating, so
            // it must start a new AssistantText entry (rendered below the
            // just-inserted SourceHeading) rather than being appended to the
            // previous agent's text.  Without this reset, sub-agent message
            // chunks get concatenated onto the parent's AssistantText,
            // producing a single run-on paragraph that mixes content from
            // multiple agents on the top-level row.
            self.app.streaming_open = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::types::TranscriptItem;

    #[tokio::test]
    async fn completing_assistant_stream_appends_markdown_links() {
        let config = crate::tests::test_config();
        let mut tui = crate::types::Tui::init(&config).await.unwrap();
        tui.finish_main_prompt_final(
            "Read [Harnx](https://harnx.dev)".into(),
            &Default::default(),
        )
        .await;
        assert!(matches!(
            tui.app.transcript.last(),
            Some(crate::types::TranscriptItem::MarkdownLink { text, url })
                if text == "Harnx" && url == "https://harnx.dev"
        ));
    }

    #[tokio::test]
    async fn tool_started_extracts_links_from_markdown_body() {
        use harnx_core::event::ToolKind;
        let config = crate::tests::test_config();
        let mut tui = crate::types::Tui::init(&config).await.unwrap();
        tui.handle_tui_event(crate::types::TuiEvent::LocalAgent(
            harnx_core::event::AgentEvent::Tool(harnx_core::event::ToolEvent::Started {
                id: "test-tool-1".into(),
                name: "session_prompt".into(),
                kind: ToolKind::Other,
                markdown: Some("Prompt [Session Docs](https://session.example.com)".into()),
                input: serde_json::json!({"message": "test"}),
                locations: vec![],
            }),
        ))
        .await
        .unwrap();
        // Should have ToolCall followed by MarkdownLink
        let items: Vec<_> = tui.app.transcript.iter().collect();
        assert!(items.windows(2).any(|w| matches!(
            (&w[0], &w[1]),
            (
                TranscriptItem::ToolCall { .. },
                TranscriptItem::MarkdownLink { text, url }
            ) if text == "Session Docs" && url == "https://session.example.com"
        )));
    }

    #[tokio::test]
    async fn render_sub_agent_final_extracts_markdown_link() {
        let config = crate::tests::test_config();
        let mut tui = crate::types::Tui::init(&config).await.unwrap();
        tui.render_sub_agent_final(
            "See [SubAgent Docs](https://subagent.example.com)".into(),
            &Default::default(),
        );
        // Should have AssistantText followed by MarkdownLink
        let items: Vec<_> = tui.app.transcript.iter().collect();
        assert!(items.windows(2).any(|w| matches!(
            (&w[0], &w[1]),
            (
                TranscriptItem::AssistantText { text, .. },
                TranscriptItem::MarkdownLink { text: link_text, url }
            ) if text.contains("SubAgent Docs") && link_text == "SubAgent Docs" && url == "https://subagent.example.com"
        )));
    }
}
