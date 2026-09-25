//! State transitions for interactive tool-use confirmation modals.

use crate::markdown_render::RenderedEntry;
use crate::types::{
    ConfirmView, ModalState, ToolCallBody, ToolConfirmationEvent, TranscriptItem, Tui, TuiEvent,
};
use harnx_core::tool::{ToolCall, ToolDeclaration};
use harnx_runtime::tool::render_call_for_display;
use harnx_runtime::utils::pretty_yaml_block;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use std::collections::HashMap;
use std::time::{Duration, Instant};

pub(super) const TOOL_CONFIRM_IDLE_GATE: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ConfirmDecision {
    Approve,
    RejectToAgent,
}

impl ConfirmDecision {
    const fn approved(self) -> bool {
        matches!(self, Self::Approve)
    }
}

#[derive(Debug)]
pub(super) enum ConfirmationEnqueueResult {
    Committed { activation_error: Option<String> },
    Failed(String),
    Cancelled,
}

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct TestConfirmationEnqueueRequest {
    pub session_id: String,
    pub cluster: String,
    pub message: String,
    pub submission_id: String,
}

#[cfg(test)]
pub(super) type TestConfirmationEnqueueFn = std::sync::Arc<
    dyn Fn(
            TestConfirmationEnqueueRequest,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = ConfirmationEnqueueResult> + Send>>
        + Send
        + Sync,
>;

/// Reply channel for tool confirmation: uses async tokio oneshot for NATS
/// (worker-side) requests, allowing cancellation when the worker stops waiting.
pub(crate) enum ToolConfirmationReply {
    Routed {
        reply: tokio::sync::oneshot::Sender<bool>,
        closed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    },
}

impl ToolConfirmationReply {
    fn is_closed(&self) -> bool {
        match self {
            Self::Routed { reply, closed } => {
                reply.is_closed() || closed.load(std::sync::atomic::Ordering::Acquire)
            }
        }
    }

    pub(crate) fn send(self, approved: bool) -> Result<(), bool> {
        let approved = approved && !self.is_closed();
        match self {
            Self::Routed { reply, .. } => reply.send(approved),
        }
    }
}

fn confirmation_header(tool_name: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!("Allow tool '{tool_name}'?"),
        Style::default()
            .fg(Color::Reset)
            .add_modifier(Modifier::BOLD),
    ))
}

fn confirmation_parts(modal: &ModalState) -> Option<(&str, &serde_json::Value, Option<&str>)> {
    match modal {
        ModalState::ConfirmToolUse(state) => {
            Some((&state.tool_name, &state.arguments, state.reason.as_deref()))
        }
        _ => None,
    }
}

fn declaration_map(declarations: Vec<ToolDeclaration>) -> HashMap<String, ToolDeclaration> {
    declarations
        .into_iter()
        .map(|declaration| (declaration.name.clone(), declaration))
        .collect()
}

fn resolve_call_template(
    tool_name: &str,
    arguments: &serde_json::Value,
    declarations: &HashMap<String, ToolDeclaration>,
) -> (bool, Option<String>) {
    let has_template = declarations
        .get(tool_name)
        .is_some_and(|declaration| declaration.call_template.is_some());
    if !has_template {
        return (false, None);
    }

    let raw_fallback = pretty_yaml_block(arguments);
    let call = ToolCall::new(tool_name.to_string(), arguments.clone(), None, None);
    let rendered = render_call_for_display(&call, arguments, &raw_fallback, declarations);
    (true, rendered)
}

fn tool_call_body(
    arguments: &serde_json::Value,
    view: ConfirmView,
    has_template: bool,
    template_text: Option<&str>,
) -> ToolCallBody {
    match (view, has_template, template_text) {
        (ConfirmView::Template, true, Some(rendered)) => {
            ToolCallBody::Markdown(rendered.to_string())
        }
        _ => ToolCallBody::Yaml(pretty_yaml_block(arguments)),
    }
}

fn render_tool_call_body_entry(
    body: &ToolCallBody,
    width: u16,
    theme: Option<&syntect::highlighting::Theme>,
) -> RenderedEntry {
    let dim = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::DIM);
    let entry = match body {
        ToolCallBody::Markdown(markdown) => {
            crate::markdown_render::render_markdown(markdown, dim, width, theme)
        }
        ToolCallBody::Yaml(yaml) => crate::markdown_render::render_markdown(
            &format!("```yaml\n{yaml}\n```"),
            dim,
            width,
            theme,
        ),
    };

    if entry.total_height == 0 {
        RenderedEntry::from_lines(vec![Line::default()], width)
    } else {
        entry
    }
}

fn confirmation_footer_lines(
    state: &crate::types::ConfirmToolUseState,
    now: Instant,
) -> Vec<String> {
    let decision_help = if state.submitting {
        "Submitting… · Ctrl+C reject+interrupt".to_string()
    } else {
        let idle = now.saturating_duration_since(state.last_key_at);
        let approve = if idle < TOOL_CONFIRM_IDLE_GATE {
            let remaining = TOOL_CONFIRM_IDLE_GATE.saturating_sub(idle).as_secs_f32();
            format!("ENTER approve ({remaining:.1}s)")
        } else {
            "ENTER approve".to_string()
        };
        format!("{approve} · Ctrl+D reject · Ctrl+C reject+interrupt")
    };

    let mut editing_help = vec!["Shift/Alt+Enter newline".to_string()];
    if state.has_template {
        let target = match state.view {
            ConfirmView::Template => "raw",
            ConfirmView::RawYaml => "template",
        };
        editing_help.push(format!("Ctrl+F {target}"));
    }
    editing_help.push("PgUp/PgDn scroll".to_string());

    let mut lines = Vec::with_capacity(3);
    if let Some(error) = &state.submission_error {
        lines.push(error.clone());
    }
    lines.push(decision_help);
    lines.push(editing_help.join(" · "));
    lines
}

async fn wait_for_confirmation_cancel(
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    route: Option<crate::types::ToolConfirmationRouteHandle>,
) {
    loop {
        if cancel.load(std::sync::atomic::Ordering::Acquire)
            || route.as_ref().is_some_and(|route| route.is_closed())
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_for_prompt_abort(abort: Option<harnx_runtime::utils::AbortSignal>) {
    match abort {
        Some(abort) => harnx_runtime::utils::wait_abort_signal(&abort).await,
        None => std::future::pending().await,
    }
}

impl Tui {
    pub(super) async fn handle_tool_confirmation_event(&mut self, event: ToolConfirmationEvent) {
        match event {
            ToolConfirmationEvent::Show {
                confirmation_id,
                origin_session_id,
                cluster,
                tool_call_id,
                tool_name,
                arguments,
                reason,
                reply,
            } => {
                self.show_tool_confirmation(
                    confirmation_id,
                    origin_session_id,
                    cluster,
                    tool_call_id,
                    tool_name,
                    *arguments,
                    reason,
                    reply,
                )
                .await;
            }
            ToolConfirmationEvent::Dismiss { confirmation_id } => {
                self.dismiss_tool_confirmation(confirmation_id);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn show_tool_confirmation(
        &mut self,
        confirmation_id: u64,
        origin_session_id: String,
        cluster: String,
        tool_call_id: Option<String>,
        tool_name: String,
        arguments: serde_json::Value,
        reason: Option<String>,
        reply: ToolConfirmationReply,
    ) {
        // Route retirement is synchronous. A queued G1 Show cannot recreate a
        // modal after acceptance even if the responder has not dropped yet.
        if reply.is_closed() {
            let _ = reply.send(false);
            return;
        }
        if self.app.modal.is_some() || self.app.pending_confirm_reply.is_some() {
            let _ = reply.send(false);
            self.app.transcript.push(TranscriptItem::SystemText(
                "⚠ Tool confirmation denied because another modal is already open.".to_string(),
            ));
            self.pin_transcript_to_bottom();
            return;
        }

        let target = (origin_session_id.clone(), cluster.clone());
        let confirmation_route =
            crate::prompt::matching_tool_confirmation_route(&self.tool_confirmation_route, &target);

        let (has_template, template_text) = {
            let config = self.config.read();
            let active_package = config.active_package();
            let (declarations, _) =
                config.tool_declarations_for_use_tools(Some("*"), active_package.as_deref());
            resolve_call_template(&tool_name, &arguments, &declaration_map(declarations))
        };
        let view = if has_template {
            ConfirmView::Template
        } else {
            ConfirmView::RawYaml
        };

        // This scroll widget is bottom-oriented. Position zero with follow disabled
        // displays the beginning of a tall tool body when the modal first opens.
        let mut scroll = ratatui_widget_scrolling::ScrollState::new();
        scroll.follow = false;

        // Modal and main composer represent one draft. A pending message is
        // authoritative because the main input deliberately mirrors it while busy.
        let pending_text = self.app.pending_message.take().map(|pending| pending.text);
        if pending_text.is_some() {
            *self.shared_pending_message.lock().await = None;
        }
        let main_input_text = self.app.input.lines().join("\n");
        let draft = pending_text.unwrap_or(main_input_text);
        self.app.input = Self::new_input();

        let mut message = ratatui_textarea::TextArea::default();
        message.insert_str(draft);

        let now = Instant::now();
        self.app.pending_confirm_reply = Some(reply);
        self.app.pending_confirm_id = Some(confirmation_id);
        self.app.modal = Some(ModalState::ConfirmToolUse(Box::new(
            crate::types::ConfirmToolUseState {
                arguments,
                tool_name,
                reason,
                session_id: origin_session_id,
                cluster,
                tool_call_id,
                submission_id: uuid::Uuid::new_v4().to_string(),
                confirmation_route,
                submission_cancel: Default::default(),
                submission_error: None,
                view,
                has_template,
                template_text,
                scroll,
                message,
                opened_at: now,
                last_key_at: now,
                submitting: false,
            },
        )));
    }

    fn dismiss_tool_confirmation(&mut self, confirmation_id: u64) {
        if self.app.pending_confirm_id != Some(confirmation_id) {
            return;
        }
        self.cancel_tool_confirm();
        self.app.transcript.push(TranscriptItem::SystemText(
            "⚠ Tool confirmation was cancelled.".to_string(),
        ));
        self.pin_transcript_to_bottom();
    }

    /// Restore the modal textarea to the main composer before a cancellation
    /// path drops the modal without delivering its optional message.
    pub(super) fn restore_tool_confirmation_draft(&mut self) {
        let Some(ModalState::ConfirmToolUse(state)) = self.app.modal.as_ref() else {
            return;
        };
        let draft = state.message.lines().join("\n");
        self.set_input_text(&draft);
        self.refresh_input_chrome();
    }

    pub(super) fn cancel_tool_confirm(&mut self) {
        self.restore_tool_confirmation_draft();
        self.resolve_tool_confirm(false);
    }

    /// Submit a decision without blocking the TUI event loop. Message-bearing
    /// decisions resolve only after JetStream commits the origin-session append.
    pub(super) async fn submit_tool_confirm(&mut self, decision: ConfirmDecision) {
        let Some(ModalState::ConfirmToolUse(state)) = self.app.modal.as_mut() else {
            return;
        };
        if state.submitting {
            return;
        }

        let message = state.message.lines().join("\n");
        if message.trim().is_empty() {
            self.resolve_tool_confirm(decision.approved());
            return;
        }

        let Some(confirmation_id) = self.app.pending_confirm_id else {
            return;
        };
        state.submitting = true;
        state.submission_error = None;

        let session_id = state.session_id.clone();
        let cluster = state.cluster.clone();
        let submission_id = state.submission_id.clone();
        let confirmation_route = state.confirmation_route.clone();
        let cancel_route = confirmation_route.clone();
        let submission_cancel = std::sync::Arc::clone(&state.submission_cancel);
        let prompt_abort = self.current_prompt_abort.clone();
        let config = self.config.clone();
        let local_worker = self.local_worker.clone();
        let event_tx = self.event_tx.clone();
        #[cfg(test)]
        let enqueue_override = self.confirmation_enqueue_override.clone();

        tokio::spawn(async move {
            let enqueue = async {
                #[cfg(test)]
                if let Some(enqueue_override) = enqueue_override {
                    return enqueue_override(TestConfirmationEnqueueRequest {
                        session_id: session_id.clone(),
                        cluster: cluster.clone(),
                        message: message.clone(),
                        submission_id: submission_id.clone(),
                    })
                    .await;
                }

                let result = async {
                    let route_handle = confirmation_route.ok_or_else(|| {
                        anyhow::anyhow!("origin tool-confirmation route is no longer available")
                    })?;
                    let route = route_handle.nats().ok_or_else(|| {
                        anyhow::anyhow!("origin tool-confirmation route is not broker-backed")
                    })?;
                    crate::prompt::enqueue_text_into_target(
                        &config,
                        &local_worker,
                        session_id,
                        cluster,
                        &route,
                        &message,
                        &submission_id,
                    )
                    .await
                }
                .await;
                match result {
                    Ok(enqueued) => ConfirmationEnqueueResult::Committed {
                        activation_error: enqueued
                            .activation_error()
                            .map(|error| format!("{error:#}")),
                    },
                    Err(error) => ConfirmationEnqueueResult::Failed(format!("{error:#}")),
                }
            };

            let result = tokio::select! {
                biased;
                _ = wait_for_confirmation_cancel(
                    std::sync::Arc::clone(&submission_cancel),
                    cancel_route,
                ) => ConfirmationEnqueueResult::Cancelled,
                _ = wait_for_prompt_abort(prompt_abort) => ConfirmationEnqueueResult::Cancelled,
                result = enqueue => {
                    if submission_cancel.load(std::sync::atomic::Ordering::Acquire) {
                        ConfirmationEnqueueResult::Cancelled
                    } else {
                        result
                    }
                },
            };
            let _ = event_tx.send(TuiEvent::ToolConfirmationEnqueueFinished {
                confirmation_id,
                decision,
                result,
            });
        });
    }

    pub(super) fn finish_tool_confirmation_enqueue(
        &mut self,
        confirmation_id: u64,
        decision: ConfirmDecision,
        result: ConfirmationEnqueueResult,
    ) {
        if self.app.pending_confirm_id != Some(confirmation_id) {
            return;
        }
        let Some(ModalState::ConfirmToolUse(state)) = self.app.modal.as_mut() else {
            return;
        };

        match result {
            ConfirmationEnqueueResult::Committed { activation_error } => {
                state.submitting = false;
                let target = (state.session_id.clone(), state.cluster.clone());
                if let Some(error) = activation_error {
                    log::warn!(
                        "confirmation message committed for session {} but activation failed; resolving decision and retaining activation retry: {error}",
                        state.session_id
                    );
                    self.retain_pending_remote_activation(target);
                }
                self.resolve_tool_confirm(decision.approved());
            }
            ConfirmationEnqueueResult::Failed(error) => {
                log::warn!(
                    "failed to commit confirmation message for session {}: {error}",
                    state.session_id
                );
                state.submitting = false;
                state.submission_error = Some(format!("Message enqueue failed: {error}"));
            }
            ConfirmationEnqueueResult::Cancelled => {
                state.submitting = false;
            }
        }
    }

    /// Resolve an in-flight tool-use confirmation: send the decision to the
    /// worker-side async task and dismiss the modal.
    pub(super) fn resolve_tool_confirm(&mut self, allow: bool) {
        if let Some(ModalState::ConfirmToolUse(state)) = self.app.modal.as_ref() {
            state
                .submission_cancel
                .store(true, std::sync::atomic::Ordering::Release);
        }
        if let Some(reply) = self.app.pending_confirm_reply.take() {
            let _ = reply.send(allow);
        }
        self.app.pending_confirm_id = None;
        self.app.modal = None;
    }

    pub(super) fn render_tool_confirm_overlay(
        &self,
        frame: &mut Frame<'_>,
        area: ratatui::layout::Rect,
        modal: &mut ModalState,
    ) {
        self.render_tool_confirm_modal(frame, area, modal);
    }

    pub(super) fn confirm_tool_modal_height(&self, width: u16, modal: &ModalState) -> u16 {
        let Some((_, arguments, reason)) = confirmation_parts(modal) else {
            return 0;
        };
        let ModalState::ConfirmToolUse(state) = modal else {
            return 0;
        };
        let content_width = width.saturating_sub(2);
        if content_width == 0 {
            return 6;
        }

        let pinned_height = self.confirmation_pinned_height(content_width, reason);
        let body = tool_call_body(
            arguments,
            state.view,
            state.has_template,
            state.template_text.as_deref(),
        );
        let body_height =
            render_tool_call_body_entry(&body, content_width, self.code_theme.as_ref())
                .total_height
                .saturating_add(1); // "Input:" label

        // Borders + pinned header/reason + body + message textarea + footer.
        2u16.saturating_add(pinned_height)
            .saturating_add(body_height.max(1))
            .saturating_add(3)
            .saturating_add(confirmation_footer_lines(state, Instant::now()).len() as u16)
    }

    fn confirmation_pinned_height(&self, width: u16, reason: Option<&str>) -> u16 {
        let mut height = 2u16; // header + spacer
        if let Some(reason) = reason.filter(|reason| !reason.is_empty()) {
            let entry = crate::markdown_render::render_markdown(
                reason,
                Style::default(),
                width,
                self.code_theme.as_ref(),
            );
            height = height
                .saturating_add(1) // "Reason:" label
                .saturating_add(entry.total_height);
        }
        height
    }

    /// Render header/reason, tool body, message input, and footer in separate
    /// regions so scrolling the tool body cannot move the other regions.
    pub(super) fn render_tool_confirm_modal(
        &self,
        frame: &mut Frame<'_>,
        area: ratatui::layout::Rect,
        modal: &mut ModalState,
    ) {
        frame.render_widget(ratatui::widgets::Clear, area);

        let block = Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(
                "Tool confirmation",
                Style::default().fg(Color::Yellow),
            ))
            .border_style(Style::default().fg(Color::Yellow));
        let inner_area = block.inner(area);
        frame.render_widget(block, area);

        if inner_area.height == 0 || inner_area.width == 0 {
            return;
        }

        let ModalState::ConfirmToolUse(state) = modal else {
            return;
        };
        let pinned_height =
            self.confirmation_pinned_height(inner_area.width, state.reason.as_deref());
        let footer_height =
            (confirmation_footer_lines(state, Instant::now()).len() as u16).min(inner_area.height);
        let remaining = inner_area.height.saturating_sub(footer_height);
        let message_height = 3.min(remaining.saturating_sub(1));
        let header_and_body_height = remaining.saturating_sub(message_height);
        let header_height = if header_and_body_height > 1 {
            pinned_height.min(header_and_body_height - 1)
        } else {
            header_and_body_height
        };
        let body_height = header_and_body_height.saturating_sub(header_height);

        let header_area =
            ratatui::layout::Rect::new(inner_area.x, inner_area.y, inner_area.width, header_height);
        let body_area = ratatui::layout::Rect::new(
            inner_area.x,
            header_area.bottom(),
            inner_area.width,
            body_height,
        );
        let message_area = ratatui::layout::Rect::new(
            inner_area.x,
            body_area.bottom(),
            inner_area.width,
            message_height,
        );
        let footer_area = ratatui::layout::Rect::new(
            inner_area.x,
            message_area.bottom(),
            inner_area.width,
            footer_height,
        );

        self.render_confirmation_pinned(
            frame,
            header_area,
            &state.tool_name,
            state.reason.as_deref(),
        );

        if body_area.height > 0 {
            let body = tool_call_body(
                &state.arguments,
                state.view,
                state.has_template,
                state.template_text.as_deref(),
            );
            let entries = vec![
                RenderedEntry::from_lines(
                    vec![Line::from(Span::styled(
                        "Input:",
                        Style::default().fg(Color::DarkGray),
                    ))],
                    body_area.width,
                ),
                render_tool_call_body_entry(&body, body_area.width, self.code_theme.as_ref()),
            ];
            state.scroll.render(frame, body_area, &entries, |entry| {
                (entry.total_height as usize, entry.clone())
            });
            if !state.scroll.follow {
                state.scroll.position = state.scroll.position.min(state.scroll.last_max_position);
            }
        }

        if message_area.height > 0 {
            state.message.set_block(
                Block::default()
                    .borders(Borders::TOP)
                    .title("Message (optional)")
                    .border_style(Style::default().fg(Color::DarkGray)),
            );
            state.message.set_cursor_line_style(Style::default());
            state
                .message
                .set_wrap_mode(ratatui_textarea::WrapMode::Word);
            frame.render_widget(&state.message, message_area);
        }

        let footer = confirmation_footer_lines(state, Instant::now())
            .into_iter()
            .map(|line| Line::from(Span::styled(line, Style::default().fg(Color::DarkGray))))
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(footer), footer_area);
    }

    fn render_confirmation_pinned(
        &self,
        frame: &mut Frame<'_>,
        area: ratatui::layout::Rect,
        tool_name: &str,
        reason: Option<&str>,
    ) {
        if area.height == 0 {
            return;
        }

        let header_area = ratatui::layout::Rect::new(area.x, area.y, area.width, 1);
        frame.render_widget(Paragraph::new(confirmation_header(tool_name)), header_area);

        let Some(reason) = reason.filter(|reason| !reason.is_empty()) else {
            return;
        };
        if area.height <= 2 {
            return;
        }

        let label_area = ratatui::layout::Rect::new(area.x, area.y + 2, area.width, 1);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Reason:",
                Style::default().fg(Color::DarkGray),
            ))),
            label_area,
        );
        if area.height <= 3 {
            return;
        }

        let entry = crate::markdown_render::render_markdown(
            reason,
            Style::default(),
            area.width,
            self.code_theme.as_ref(),
        );
        let reason_area = ratatui::layout::Rect::new(
            area.x,
            area.y + 3,
            area.width,
            area.height.saturating_sub(3),
        );
        frame.render_widget(entry, reason_area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn declaration(name: &str, template: Option<&str>) -> ToolDeclaration {
        ToolDeclaration {
            name: name.to_string(),
            description: String::new(),
            parameters: Default::default(),
            mcp_tool_name: Some(name.to_string()),
            mcp_server_name: None,
            call_template: template.map(str::to_string),
            result_template: None,
            idempotent_hint: None,
            read_only_hint: None,
        }
    }

    #[test]
    fn resolves_and_renders_declared_call_template() {
        let declarations = declaration_map(vec![declaration(
            "bash_exec",
            Some("run `${{ args.command }}`"),
        )]);
        let arguments = json!({"command": "ls -la"});

        let (has_template, rendered) =
            resolve_call_template("bash_exec", &arguments, &declarations);

        assert!(has_template);
        assert_eq!(rendered.as_deref(), Some("run `$ls -la`"));
        assert_eq!(
            tool_call_body(
                &arguments,
                ConfirmView::Template,
                has_template,
                rendered.as_deref(),
            ),
            ToolCallBody::Markdown("run `$ls -la`".to_string())
        );
    }

    #[test]
    fn raw_yaml_view_ignores_available_template() {
        let arguments = json!({"command": "ls -la", "lines": [1, 2]});

        let body = tool_call_body(
            &arguments,
            ConfirmView::RawYaml,
            true,
            Some("template output"),
        );

        let ToolCallBody::Yaml(yaml) = body else {
            panic!("raw view must render YAML");
        };
        assert!(yaml.contains("command: ls -la"));
        assert!(yaml.contains("lines:"));
        assert!(!yaml.contains("template output"));
    }

    #[test]
    fn missing_template_falls_back_to_yaml() {
        let arguments = json!({"path": "README.md"});
        let declarations = declaration_map(vec![declaration("fs_read", None)]);

        let (has_template, rendered) = resolve_call_template("fs_read", &arguments, &declarations);
        let body = tool_call_body(
            &arguments,
            ConfirmView::Template,
            has_template,
            rendered.as_deref(),
        );

        assert!(!has_template);
        assert!(rendered.is_none());
        assert_eq!(body, ToolCallBody::Yaml(pretty_yaml_block(&arguments)));
    }

    #[test]
    fn null_and_empty_arguments_render_without_panicking() {
        for arguments in [json!(null), json!({}), json!([])] {
            let body = tool_call_body(&arguments, ConfirmView::RawYaml, false, None);
            let ToolCallBody::Yaml(yaml) = &body else {
                panic!("arguments without a template must render as YAML");
            };
            assert_eq!(yaml, &pretty_yaml_block(&arguments));
            assert!(render_tool_call_body_entry(&body, 40, None).total_height > 0);
        }
    }

    #[test]
    fn raw_yaml_keeps_arguments_beyond_legacy_preview_limit() {
        let tail = "end-of-full-argument";
        let value = format!("{}{tail}", "x".repeat(200));
        let arguments = json!({"prompt": value});

        let ToolCallBody::Yaml(yaml) =
            tool_call_body(&arguments, ConfirmView::RawYaml, false, None)
        else {
            panic!("arguments without a template must render as YAML");
        };

        assert!(yaml.contains(tail));
        assert!(yaml.len() > 160);
    }
}
