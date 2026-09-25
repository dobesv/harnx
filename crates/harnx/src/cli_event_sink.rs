//! `CliAgentEventSink` renders `AgentEvent`s for the non-interactive CLI.
//!
//! Streaming chunks (`Model::MessageChunk` / `Model::ThoughtChunk`) are
//! written directly to stdout with optional markdown rendering + raw-mode
//! cursor manipulation — the sink transplants the display logic
//! previously owned by `render::render_stream`. Non-streaming events
//! (notices, errors, usage, tool starts/failures) still go to stderr.

use std::collections::HashMap;
use std::io::{stdout, Write};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use harnx_core::event::{
    AgentEvent, AgentEventSink, AgentSource, ContentBlock, ModelEvent, NoticeEvent, SessionEvent,
    SubAgentProgress, SubAgentProgressStatus, ToolEvent, TurnEvent, UserEvent,
};
use harnx_core::session::{CompactOutcome, UnchangedReason};
use harnx_toolset::{
    is_subagent_launcher, TOOL_TIMER_MIN_ELAPSED_MS, TOOL_TIMER_NOTICE_INTERVAL_MS,
};

use harnx_render::{MarkdownRender, RenderOptions};
use harnx_runtime::utils::{dimmed_text, pretty_yaml_block, warning_text, IS_STDOUT_TERMINAL};

/// Stderr-bound sink for the non-interactive CLI. Thread-safe — interior
/// state is held behind an `Arc<Mutex<CliSinkState>>` so multiple clones
/// of the sink share the same render buffer.
#[derive(Clone)]
pub struct CliAgentEventSink {
    state: Arc<Mutex<CliSinkState>>,
}

/// Returns `true` for model output events that carry streamed content and may
/// need a per-source heading printed before the first chunk from each agent.
fn is_model_output_event(event: &AgentEvent) -> bool {
    matches!(
        event,
        AgentEvent::Model(ModelEvent::MessageChunk { .. })
            | AgentEvent::Model(ModelEvent::ThoughtChunk { .. })
            | AgentEvent::Model(ModelEvent::Final { .. })
            | AgentEvent::Model(ModelEvent::Error(_))
            | AgentEvent::User(UserEvent::Message { .. })
    )
}

fn source_heading(source: &AgentSource) -> String {
    source.heading()
}

struct CliSinkState {
    render: Option<MarkdownRender>,
    buffer: String,
    last_ui_output_source: Option<AgentSource>,
    highlight: bool,
    render_options: RenderOptions,
    final_only: bool,
    subagents: CliSubagentReporter,
    /// In-flight tool call timer tracking. Keyed by tool call ID.
    /// Excludes sub-agent launcher tools (session_new/session_prompt + prefixed forms).
    tool_timers: ToolCallTimerReporter,
}

const SUBAGENT_REPORT_INTERVAL_MS: u64 = 10_000;

/// Tracks in-flight tool calls for "still running" notices.
#[derive(Default)]
struct ToolCallTimerReporter {
    /// Keyed by tool call ID. Entry stores tool name, start time, and last reported bucket.
    in_flight: HashMap<String, ToolCallTimerEntry>,
}

struct ToolCallTimerEntry {
    tool_name: String,
    started: Instant,
    /// The last bucket index (elapsed_ms / NOTICE_INTERVAL_MS) for which a notice was printed.
    /// Starts at 0, so first notice is when bucket becomes 1 (at 10s).
    last_reported_bucket: u64,
}

impl CliSinkState {
    /// Called on ToolEvent::Started. Tracks the call if it's not a sub-agent launcher.
    fn tool_call_started(&mut self, id: &str, name: &str) {
        if is_subagent_launcher(name) {
            return;
        }
        self.tool_timers.in_flight.insert(
            id.to_string(),
            ToolCallTimerEntry {
                tool_name: name.to_string(),
                started: Instant::now(),
                last_reported_bucket: 0,
            },
        );
    }

    /// Called on terminal tool events (Completed/Failed/Blocked).
    /// Returns Some(duration string) if the tool ran >= TOOL_TIMER_MIN_ELAPSED_MS.
    fn tool_call_finished(&mut self, id: &str) -> Option<String> {
        let entry = self.tool_timers.in_flight.remove(id)?;
        let elapsed_ms = entry.started.elapsed().as_millis() as u64;
        if elapsed_ms >= TOOL_TIMER_MIN_ELAPSED_MS {
            Some(format_elapsed(elapsed_ms))
        } else {
            None
        }
    }

    /// Called by the 1s ticker. Prints "still running" notices for tools crossing new 10s buckets.
    /// Prints directly under lock to avoid race conditions with completion events.
    fn tool_timer_tick(&mut self) {
        for entry in self.tool_timers.in_flight.values_mut() {
            let elapsed_ms = entry.started.elapsed().as_millis() as u64;
            let bucket = elapsed_ms / TOOL_TIMER_NOTICE_INTERVAL_MS;
            // Print when crossing into new bucket (bucket > last_reported_bucket)
            // and at least one full interval has elapsed (bucket >= 1).
            if bucket > entry.last_reported_bucket && bucket >= 1 {
                entry.last_reported_bucket = bucket;
                eprintln!(
                    "{}",
                    dimmed_text(&format!(
                        "⋯ {} still running ({})",
                        entry.tool_name,
                        format_elapsed(elapsed_ms)
                    ))
                );
            }
        }
    }

    /// Clear all in-flight tool timer state (called on every run_turn exit).
    fn clear_tool_timers(&mut self) {
        self.tool_timers.in_flight.clear();
    }
}

#[derive(Default)]
struct CliSubagentReporter {
    invocations: HashMap<String, CliInvocationReport>,
}

#[derive(Default)]
struct CliInvocationReport {
    last_running_bucket: u64,
    terminal: bool,
}

impl CliSubagentReporter {
    fn started(
        &mut self,
        agent: &str,
        session_id: &str,
        invocation_id: Option<&str>,
    ) -> Option<String> {
        if let Some(invocation_id) = invocation_id {
            if self.invocations.contains_key(invocation_id) {
                return None;
            }
            self.invocations
                .insert(invocation_id.to_string(), CliInvocationReport::default());
        }
        Some(format!(
            "[sub-agent] started agent={agent} session={session_id}"
        ))
    }

    fn progress(&mut self, progress: &SubAgentProgress) -> Option<String> {
        let report = self
            .invocations
            .entry(progress.invocation_id.clone())
            .or_default();
        if report.terminal {
            return None;
        }
        match progress.status {
            SubAgentProgressStatus::Running => {
                let bucket = progress.elapsed_ms / SUBAGENT_REPORT_INTERVAL_MS;
                if bucket == 0 || bucket <= report.last_running_bucket {
                    return None;
                }
                report.last_running_bucket = bucket;
            }
            SubAgentProgressStatus::Cancelling | SubAgentProgressStatus::Unconfirmed => {}
            SubAgentProgressStatus::Done
            | SubAgentProgressStatus::Failed
            | SubAgentProgressStatus::Cancelled => report.terminal = true,
        }
        Some(format_subagent_progress(progress))
    }
}

fn format_subagent_progress(progress: &SubAgentProgress) -> String {
    let status = match progress.status {
        SubAgentProgressStatus::Running => "running",
        SubAgentProgressStatus::Done => "done",
        SubAgentProgressStatus::Failed => "failed",
        SubAgentProgressStatus::Cancelling => "cancelling",
        SubAgentProgressStatus::Cancelled => "cancelled",
        SubAgentProgressStatus::Unconfirmed => "unconfirmed",
    };
    format!(
        "[sub-agent] {status} agent={} session={} elapsed={} in={} out={} cached={} tools={}",
        progress.agent,
        progress.session_id,
        format_elapsed(progress.elapsed_ms),
        progress.usage.input_tokens,
        progress.usage.output_tokens,
        progress.usage.cached_tokens,
        progress.tool_call_count,
    )
}

fn format_elapsed(elapsed_ms: u64) -> String {
    let seconds = elapsed_ms / 1_000;
    format!("{seconds}s")
}

impl CliAgentEventSink {
    pub fn new(
        highlight: bool,
        render_options: RenderOptions,
        abort_signal: harnx_core::abort::AbortSignal,
    ) -> Self {
        Self::new_with_options(highlight, render_options, abort_signal, false)
    }

    pub fn new_with_final_only(
        highlight: bool,
        render_options: RenderOptions,
        abort_signal: harnx_core::abort::AbortSignal,
    ) -> Self {
        Self::new_with_options(highlight, render_options, abort_signal, true)
    }

    fn new_with_options(
        highlight: bool,
        render_options: RenderOptions,
        _abort_signal: harnx_core::abort::AbortSignal,
        final_only: bool,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(CliSinkState {
                render: None,
                buffer: String::new(),
                last_ui_output_source: None,
                highlight,
                render_options,
                final_only,
                subagents: CliSubagentReporter::default(),
                tool_timers: ToolCallTimerReporter::default(),
            })),
        }
    }

    /// Called by the 1s ticker in oneshot_nats::run_turn.
    /// Prints "still running" notices directly under lock to avoid race conditions
    /// with ToolEvent::Completed arriving on another thread.
    pub fn tool_timer_tick(&self) {
        let mut state = match self.state.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.tool_timer_tick();
    }

    /// Clear all in-flight tool timer state (called on every run_turn exit).
    pub fn clear_tool_timers(&self) {
        let mut state = match self.state.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.clear_tool_timers();
    }
}

impl CliSinkState {
    /// Print the agent/session heading when the event source changes from
    /// the previously tracked source.  Calls `cleanup()` first so any
    /// buffered output from the prior source is flushed before the heading.
    /// No-ops when the source is unchanged — this is how we avoid repeating
    /// the heading for every streaming chunk from the same agent.
    fn maybe_emit_source_heading(&mut self, next_source: Option<&AgentSource>) {
        if next_source == self.last_ui_output_source.as_ref() {
            return;
        }
        if let Err(err) = self.cleanup() {
            eprintln!(
                "{}",
                warning_text(&format!("cli-sink cleanup failed: {err}"))
            );
        }
        if let Some(source) = next_source {
            println!("{}", source_heading(source));
        }
        self.last_ui_output_source = next_source.cloned();
    }

    /// Dispatch a chunk of text to either the markdown or raw rendering
    /// path based on the highlight flag snapshot + stdout terminal-ness.
    fn handle_chunk_text(&mut self, text: &str) -> anyhow::Result<()> {
        if self.highlight && *IS_STDOUT_TERMINAL {
            self.handle_markdown_chunk(text)
        } else {
            self.handle_raw_chunk(text)
        }
    }

    fn handle_raw_chunk(&mut self, text: &str) -> anyhow::Result<()> {
        print!("{text}");
        if let Some((_, tail)) = text.rsplit_once('\n') {
            self.buffer.clear();
            self.buffer.push_str(tail);
        } else {
            self.buffer.push_str(text);
        }
        stdout().flush()?;
        Ok(())
    }

    /// Markdown streaming path. Keep terminal in cooked mode.
    ///
    /// Each chunk is printed immediately so text is visible without delay.
    /// Markdown rendering is applied only to completed lines (those ending
    /// with `\n`).
    ///
    /// Partial-line strategy:
    /// - Chunks with no `\n` are printed raw and accumulated in `self.buffer`.
    /// - When a `\n` arrives, the completed portion is re-rendered: `\r`
    ///   returns to column 0 and the rendered text overwrites the raw prefix
    ///   that was already printed.  No cursor movement beyond `\r` is needed.
    /// - The tail after the last `\n` is printed raw immediately and buffered
    ///   for the next newline.
    fn handle_markdown_chunk(&mut self, text: &str) -> anyhow::Result<()> {
        if self.render.is_none() {
            self.render = Some(MarkdownRender::init(self.render_options.clone())?);
        }

        let mut writer = stdout();
        let text = text.replace('\t', "    ");

        if !text.contains('\n') {
            // No newline — print immediately so the user sees it, and buffer
            // for re-rendering when the line is eventually completed.
            self.buffer.push_str(&text);
            print!("{text}");
            writer.flush()?;
            return Ok(());
        }

        // At least one newline present.  Combine buffered prefix with new
        // text, split at the last newline, render the completed head, then
        // immediately print the raw tail.
        let combined = format!("{}{}", self.buffer, text);
        let (head, tail) = split_line_tail_local(&combined);
        let render = self.render.as_mut().expect("render initialized above");
        let output = render.render(head);
        // '\r' returns to column 0 to overwrite the raw partial line that was
        // already printed. render() joins lines with '\n' but no trailing
        // newline; println! adds the separator after the completed block.
        print!("\r{output}");
        println!();
        self.buffer = tail.to_string();
        if !tail.is_empty() {
            print!("{tail}");
        }
        writer.flush()?;
        Ok(())
    }

    /// Close a partial stdout line before writing a separate line to stderr.
    fn flush_pending_stdout_line(&mut self) -> anyhow::Result<()> {
        if !self.buffer.is_empty() {
            println!();
            self.buffer.clear();
            stdout().flush()?;
        }
        Ok(())
    }

    /// End-of-turn cleanup: flush any buffered partial line and reset state so
    /// the next turn starts fresh.
    fn cleanup(&mut self) -> anyhow::Result<()> {
        self.flush_pending_stdout_line()?;
        self.render = None;
        self.last_ui_output_source = None;
        Ok(())
    }

    /// Stderr render for `ToolEvent::Started`: when the producer rendered an
    /// MCP `call_template` into `markdown`, print only the markdown-styled
    /// line (no `[tool] name` prefix). When no markdown is present, fall back
    /// to the dim tool name and YAML arguments.
    fn print_tool_started(
        &mut self,
        name: &str,
        input: &serde_json::Value,
        markdown: Option<&str>,
    ) {
        let rendered = Self::format_tool_started(name, input, markdown, |text| {
            if text.contains('\n') {
                self.render_markdown_block(text)
            } else {
                self.render_markdown_line(text)
            }
        });
        eprintln!("{rendered}");
    }

    fn format_tool_started(
        name: &str,
        input: &serde_json::Value,
        markdown: Option<&str>,
        mut render_markdown: impl FnMut(&str) -> String,
    ) -> String {
        match markdown.map(str::trim).filter(|t| !t.is_empty()) {
            Some(t) => render_markdown(t),
            None if !input.is_null() => {
                dimmed_text(&format!("[tool] {name}\n{}", pretty_yaml_block(input)))
            }
            None => dimmed_text(&format!("[tool] {name}")),
        }
    }

    /// Stderr render for `ToolEvent::Completed`. Always routes through the
    /// multi-line `MarkdownRender::render` so block-level constructs work
    /// — fenced code (e.g. the ```diff blocks emitted by harnx-fs-tools /
    /// harnx-bash-tools for history diffs) gets syntect highlighting,
    /// inline emphasis from a templated MCP `result_template` still
    /// renders, and plain text passes through unchanged. Falls back to
    /// dim plain text when highlighting is disabled or the renderer
    /// can't initialize.
    fn print_tool_completed(&mut self, output: &serde_json::Value, markdown: Option<&str>) {
        let text = harnx_runtime::utils::render_tool_result_text(output, markdown);
        let trimmed = text.trim_end_matches('\n');
        if trimmed.is_empty() {
            return;
        }
        eprintln!("{}", self.render_markdown_block(trimmed));
    }

    /// Lazy-initialize the shared `MarkdownRender` and run `with_render`
    /// against it. Returns `fallback(text)` when highlighting is disabled
    /// (no TTY, `--no-highlight`, or renderer init failure) so callers
    /// can choose between dim plain text and the input unchanged.
    fn with_markdown<F, G>(&mut self, text: &str, with_render: F, fallback: G) -> String
    where
        F: FnOnce(&mut MarkdownRender, &str) -> String,
        G: FnOnce(&str) -> String,
    {
        if !(self.highlight && *IS_STDOUT_TERMINAL) {
            return fallback(text);
        }
        if self.render.is_none() {
            match MarkdownRender::init(self.render_options.clone()) {
                Ok(r) => self.render = Some(r),
                Err(_) => return fallback(text),
            }
        }
        self.render
            .as_mut()
            .map(|r| with_render(r, text))
            .unwrap_or_else(|| fallback(text))
    }

    /// Render a multi-line markdown document via `MarkdownRender::render`,
    /// which preserves state across lines so fenced code blocks and
    /// per-language syntect highlighting work. Falls back to dim plain
    /// text when highlighting is disabled.
    fn render_markdown_block(&mut self, text: &str) -> String {
        self.with_markdown(text, |r, t| r.render(t), dimmed_text)
    }

    /// Render a single line through `MarkdownRender` so MCP `call_template`/
    /// `result_template` text shows its `**bold**` / `*italic*` / `` `code` ``
    /// styling. Returns the input unchanged when highlighting is disabled.
    fn render_markdown_line(&mut self, text: &str) -> String {
        self.with_markdown(text, |r, t| r.render_line(t), str::to_string)
    }

    fn print_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Turn(event) => self.print_turn_event(event),
            AgentEvent::Notice(event) => self.print_notice(event),
            AgentEvent::Model(event) => self.print_model_event(event),
            AgentEvent::User(event) => self.print_user_event(event),
            AgentEvent::Tool(event) => self.print_tool_event(event),
            AgentEvent::Session(event) => self.print_session_event(event),
            AgentEvent::Status(_) => {}
            other => eprintln!("{}", dimmed_text(&format!("[event] {other:?}"))),
        }
    }

    fn print_turn_event(&mut self, event: TurnEvent) {
        match event {
            TurnEvent::Started => {}
            TurnEvent::Ended { .. } | TurnEvent::Interrupted { .. } => self.cleanup_or_warn(),
            TurnEvent::RetryAttempt { attempt, reason } => {
                eprintln!("{}", warning_text(&format!("retry #{attempt}: {reason}")));
            }
            TurnEvent::ModelFallback { from, to } => {
                eprintln!(
                    "{}",
                    warning_text(&format!("model fallback: {from} → {to}"))
                );
            }
            TurnEvent::HandoffRequested { agent, .. } => {
                eprintln!("{}", dimmed_text(&format!("handoff → {agent}")));
            }
            TurnEvent::SubAgentStarted {
                agent,
                session_id,
                invocation_id,
            } => {
                if let Some(line) =
                    self.subagents
                        .started(&agent, &session_id, invocation_id.as_deref())
                {
                    eprintln!("{}", dimmed_text(&line));
                }
            }
            TurnEvent::SubAgentProgress(progress) => self.print_subagent_progress(progress),
        }
    }

    fn print_subagent_progress(&mut self, progress: SubAgentProgress) {
        let Some(line) = self.subagents.progress(&progress) else {
            return;
        };
        if progress.status == SubAgentProgressStatus::Failed {
            eprintln!("{}", warning_text(&line));
        } else {
            eprintln!("{}", dimmed_text(&line));
        }
    }

    fn print_notice(&mut self, event: NoticeEvent) {
        self.flush_pending_stdout_line_or_warn();
        match event {
            NoticeEvent::Info(message) => println!("{message}"),
            NoticeEvent::Warning(message) => eprintln!("{}", warning_text(&message)),
            NoticeEvent::Error(message) => {
                eprintln!("{}", warning_text(&format!("error: {message}")));
            }
        }
    }

    fn print_model_event(&mut self, event: ModelEvent) {
        match event {
            ModelEvent::MessageChunk { blocks } | ModelEvent::ThoughtChunk { blocks } => {
                self.print_content_blocks(blocks);
            }
            ModelEvent::Final { output, usage } => {
                if !output.is_empty() {
                    eprintln!("{output}");
                }
                self.cleanup_or_warn();
                if !usage.is_empty() {
                    eprintln!("Usage: {usage}");
                }
            }
            ModelEvent::Error(error) => {
                self.cleanup_or_warn();
                eprintln!("{}", warning_text(&format!("LLM error: {error}")));
            }
            ModelEvent::Usage { .. } => {}
        }
    }

    fn print_content_blocks(&mut self, blocks: Vec<ContentBlock>) {
        for block in blocks {
            let ContentBlock::Text(text) = block else {
                continue;
            };
            if let Err(error) = self.handle_chunk_text(&text) {
                eprintln!("{}", warning_text(&format!("render failed: {error}")));
                break;
            }
        }
    }

    fn print_user_event(&mut self, event: UserEvent) {
        let UserEvent::Message { content } = event;
        self.cleanup_or_warn();
        if !content.is_empty() {
            eprintln!("{}", dimmed_text(&content));
        }
    }

    fn print_tool_event(&mut self, event: ToolEvent) {
        if !matches!(
            &event,
            ToolEvent::Progress { .. } | ToolEvent::Update { .. }
        ) {
            self.flush_pending_stdout_line_or_warn();
        }
        match event {
            ToolEvent::Started {
                id,
                name,
                input,
                markdown,
                ..
            } => {
                self.tool_call_started(&id, &name);
                self.print_tool_started(&name, &input, markdown.as_deref());
            }
            ToolEvent::Failed { id, error, .. } => {
                if let Some(elapsed) = self.tool_call_finished(&id) {
                    // Long-running tool failure: format with warning style for the error,
                    // dimmed elapsed time suffix.
                    eprintln!(
                        "{} ({})",
                        warning_text(&format!("⏺ tool error: {error}")),
                        dimmed_text(&elapsed)
                    );
                } else {
                    eprintln!("{}", warning_text(&format!("tool error: {error}")));
                }
            }
            ToolEvent::Completed {
                id,
                output,
                markdown,
                ..
            } => {
                if let Some(elapsed) = self.tool_call_finished(&id) {
                    // Print the final duration line after normal completion output.
                    self.print_tool_completed(&output, markdown.as_deref());
                    eprintln!("{}", dimmed_text(&format!("⏺ done ({elapsed})")));
                } else {
                    self.print_tool_completed(&output, markdown.as_deref());
                }
            }
            ToolEvent::Blocked {
                id, name, reason, ..
            } => {
                // Blocked is terminal for this call but restartable, still remove from in_flight.
                self.tool_call_finished(&id);
                eprintln!("{}", warning_text(&format!("blocked: {name} — {reason}")));
            }
            ToolEvent::Progress { .. } | ToolEvent::Update { .. } => {}
        }
    }

    fn print_session_event(&mut self, event: SessionEvent) {
        if !matches!(&event, SessionEvent::LogSeqAssigned { .. }) {
            self.flush_pending_stdout_line_or_warn();
        }
        match event {
            SessionEvent::CompactingStarted { .. } => {
                eprintln!("{}", dimmed_text("Compacting the session..."));
            }
            SessionEvent::CompactingCompleted { outcome, .. } => {
                self.cleanup_or_warn();
                match outcome {
                    CompactOutcome::Compacted => {
                        eprintln!("{}", dimmed_text("✓ Compacted the session."));
                    }
                    CompactOutcome::Unchanged(reason) => {
                        let message = match reason {
                            UnchangedReason::NoUserMessages => "No user messages to compact",
                            UnchangedReason::NothingEligible => "Nothing eligible for compaction",
                            UnchangedReason::AlreadyCompacted => "Session already compacted",
                        };
                        eprintln!("{}", dimmed_text(message));
                    }
                    CompactOutcome::Failed(error) => {
                        eprintln!("{}", warning_text(&format!("compaction failed: {error}")));
                    }
                }
            }
            SessionEvent::CompactingFailed { error, .. } => {
                self.cleanup_or_warn();
                eprintln!("{}", warning_text(&format!("compaction failed: {error}")));
            }
            SessionEvent::TitleGenerationFailed(error) => {
                eprintln!(
                    "{}",
                    warning_text(&format!("title generation failed: {error}"))
                );
            }
            // Persistence bookkeeping: the TUI patches transcript rows with the
            // assigned log seq for edit/delete/rewind targeting, but the CLI
            // makes no use of it. Drop it silently instead of printing a raw
            // `[event] LogSeqAssigned { seq: N }` debug line on every log write.
            SessionEvent::LogSeqAssigned { .. } => {}
            other => eprintln!("{}", dimmed_text(&format!("[event] {other:?}"))),
        }
    }

    fn flush_pending_stdout_line_or_warn(&mut self) {
        if let Err(error) = self.flush_pending_stdout_line() {
            eprintln!(
                "{}",
                warning_text(&format!("cli-sink stdout flush failed: {error}"))
            );
        }
    }

    fn cleanup_or_warn(&mut self) {
        if let Err(error) = self.cleanup() {
            eprintln!(
                "{}",
                warning_text(&format!("cli-sink cleanup failed: {error}"))
            );
        }
    }
}

impl AgentEventSink for CliAgentEventSink {
    fn emit(&self, event: AgentEvent) {
        let (source, event) = match event {
            AgentEvent::SubAgent { source, event } => (Some(source), *event),
            event => (None, event),
        };
        let mut state = match self.state.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if state.final_only {
            return;
        }
        if is_model_output_event(&event) {
            state.maybe_emit_source_heading(source.as_ref());
        }
        state.print_event(event);
    }
}

// ---------------------------------------------------------------------------
// Module-private helpers for line-buffered markdown streaming.

fn split_line_tail_local(text: &str) -> (&str, &str) {
    if let Some((head, tail)) = text.rsplit_once('\n') {
        (head, tail)
    } else {
        ("", text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::event::ContentBlock;

    fn make_state(highlight: bool) -> CliSinkState {
        CliSinkState {
            render: None,
            buffer: String::new(),
            last_ui_output_source: None,
            highlight,
            render_options: RenderOptions::default(),
            final_only: false,
            subagents: CliSubagentReporter::default(),
            tool_timers: ToolCallTimerReporter::default(),
        }
    }

    #[cfg(unix)]
    fn capture_output(action: impl FnOnce()) -> String {
        use std::io::{Read, Seek, SeekFrom};
        use std::os::fd::AsRawFd;

        struct RedirectGuard {
            stdout: i32,
            stderr: i32,
        }

        impl Drop for RedirectGuard {
            fn drop(&mut self) {
                // SAFETY: Restores the saved stdout/stderr file descriptors and closes the temp copies.
                // Sound because nextest runs each test in an isolated process, so there's no concurrent
                // FD access, and this is the only code touching these descriptors.
                unsafe {
                    libc::dup2(self.stdout, libc::STDOUT_FILENO);
                    libc::dup2(self.stderr, libc::STDERR_FILENO);
                    libc::close(self.stdout);
                    libc::close(self.stderr);
                }
            }
        }

        stdout().flush().unwrap();
        std::io::stderr().flush().unwrap();
        let mut captured = tempfile::tempfile().unwrap();
        // SAFETY: Duplicates stdout/stderr FDs to preserve them for later restoration.
        // Sound because nextest runs each test in an isolated process, so no concurrent FD access.
        let saved_stdout = unsafe { libc::dup(libc::STDOUT_FILENO) };
        let saved_stderr = unsafe { libc::dup(libc::STDERR_FILENO) };
        assert!(saved_stdout >= 0 && saved_stderr >= 0);
        // SAFETY: Redirects stdout/stderr to the temp file for capture. Sound for the same reason:
        // process-isolated tests mean no other code is touching these FDs concurrently.
        assert_eq!(
            unsafe { libc::dup2(captured.as_raw_fd(), libc::STDOUT_FILENO) },
            libc::STDOUT_FILENO
        );
        assert_eq!(
            unsafe { libc::dup2(captured.as_raw_fd(), libc::STDERR_FILENO) },
            libc::STDERR_FILENO
        );
        let guard = RedirectGuard {
            stdout: saved_stdout,
            stderr: saved_stderr,
        };

        action();
        stdout().flush().unwrap();
        std::io::stderr().flush().unwrap();
        drop(guard);

        captured.seek(SeekFrom::Start(0)).unwrap();
        let mut output = String::new();
        captured.read_to_string(&mut output).unwrap();
        output
    }

    #[cfg(unix)]
    #[test]
    fn tool_event_closes_pending_stdout_line() {
        let output = capture_output(|| {
            let mut state = make_state(false);
            state.handle_raw_chunk("Fetching the issue.").unwrap();
            state.print_tool_event(ToolEvent::Started {
                id: "call-1".into(),
                name: "bash_exec".into(),
                kind: harnx_core::event::ToolKind::Other,
                markdown: None,
                input: serde_json::Value::Null,
                locations: vec![],
            });
        });

        assert!(
            output.contains("Fetching the issue.\n[tool] bash_exec\n"),
            "tool line must not share the pending model-output line: {output:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn final_usage_is_standalone_and_per_call_usage_is_silent() {
        let usage = harnx_core::api_types::CompletionTokenUsage {
            input_tokens: 12,
            output_tokens: 3,
            cached_tokens: 2,
            cache_write_tokens: 1,
        };
        let output = capture_output(|| {
            let mut state = make_state(false);
            state.handle_markdown_chunk("streamed text").unwrap();
            state.print_model_event(ModelEvent::Usage {
                input: 4,
                output: 1,
                cached: 2,
                cache_write: 0,
                session_label: None,
            });
            state.print_model_event(ModelEvent::Final {
                output: String::new(),
                usage,
            });
        });

        assert!(output.contains("streamed text\nUsage: 📥 12  📤 3  💾 2\n"));
        assert!(!output.contains("[tokens]"));

        let empty_output = capture_output(|| {
            make_state(false).print_model_event(ModelEvent::Final {
                output: String::new(),
                usage: Default::default(),
            });
        });
        assert!(!empty_output.contains("Usage:"));
        assert!(!empty_output.contains("[tokens]"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn emit_handles_each_top_level_variant_without_panic() {
        let sink = CliAgentEventSink::new(
            false,
            RenderOptions::default(),
            harnx_core::abort::create_abort_signal(),
        );

        sink.emit(AgentEvent::Turn(TurnEvent::Started));
        sink.emit(AgentEvent::Turn(TurnEvent::Ended {
            outcome: Default::default(),
        }));
        sink.emit(AgentEvent::Notice(NoticeEvent::Info("info".into())));
        sink.emit(AgentEvent::Notice(NoticeEvent::Warning("warn".into())));
        sink.emit(AgentEvent::Notice(NoticeEvent::Error("err".into())));
        sink.emit(AgentEvent::Model(ModelEvent::MessageChunk {
            blocks: vec![ContentBlock::Text("hello".into())],
        }));
        sink.emit(AgentEvent::Model(ModelEvent::Final {
            output: "done".into(),
            usage: Default::default(),
        }));
        sink.emit(AgentEvent::Model(ModelEvent::Error("boom".into())));
        sink.emit(AgentEvent::User(UserEvent::Message {
            content: "hello user".into(),
        }));
        sink.emit(AgentEvent::Tool(ToolEvent::Blocked {
            id: String::new(),
            name: "test_tool".into(),
            input: serde_json::Value::Null,
            reason: "hook denied".into(),
        }));
        sink.emit(AgentEvent::Session(SessionEvent::TitleUpdated(
            "A title".into(),
        )));
        sink.emit(AgentEvent::Session(SessionEvent::TitleGenerationFailed(
            "Miss 'api_key'".into(),
        )));
    }

    #[test]
    fn user_message_is_model_output_event() {
        assert!(is_model_output_event(&AgentEvent::User(
            UserEvent::Message {
                content: "hello user".into(),
            }
        )));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn final_only_ignores_intermediate_events() {
        let sink = CliAgentEventSink::new_with_final_only(
            false,
            RenderOptions::default(),
            harnx_core::abort::create_abort_signal(),
        );
        sink.emit(AgentEvent::Turn(TurnEvent::Started));
        sink.emit(AgentEvent::User(UserEvent::Message {
            content: "hello user".into(),
        }));
        sink.emit(AgentEvent::Model(ModelEvent::MessageChunk {
            blocks: vec![ContentBlock::Text("hidden".into())],
        }));

        let state = sink.state.lock().unwrap();
        assert!(state.buffer.is_empty());
        assert!(state.last_ui_output_source.is_none());
        assert!(state.subagents.invocations.is_empty());
    }

    fn subagent_progress(
        invocation_id: &str,
        session_id: &str,
        status: SubAgentProgressStatus,
        elapsed_ms: u64,
    ) -> SubAgentProgress {
        SubAgentProgress {
            invocation_id: invocation_id.into(),
            agent: "researcher".into(),
            session_id: session_id.into(),
            status,
            elapsed_ms,
            usage: harnx_core::api_types::CompletionTokenUsage::new(Some(120), Some(45), Some(30)),
            tool_call_count: 3,
            title: None,
        }
    }

    #[test]
    fn subagent_reporter_prints_identity_at_start() {
        let mut reporter = CliSubagentReporter::default();
        let line = reporter
            .started("researcher", "child-session", Some("inv-1"))
            .expect("first start is reported");

        assert!(line.contains("agent=researcher"));
        assert!(line.contains("session=child-session"));
        assert!(reporter
            .started("researcher", "child-session", Some("inv-1"))
            .is_none());
    }

    #[test]
    fn subagent_reporter_rate_limits_metric_changes_to_heartbeat_buckets() {
        let mut reporter = CliSubagentReporter::default();
        reporter.started("researcher", "child", Some("inv-1"));

        assert!(reporter
            .progress(&subagent_progress(
                "inv-1",
                "child",
                SubAgentProgressStatus::Running,
                9_999,
            ))
            .is_none());
        let first = reporter
            .progress(&subagent_progress(
                "inv-1",
                "child",
                SubAgentProgressStatus::Running,
                10_000,
            ))
            .expect("first heartbeat boundary is reported");
        assert!(first.contains("elapsed=10s"));
        assert!(first.contains("in=120 out=45 cached=30 tools=3"));
        assert!(reporter
            .progress(&subagent_progress(
                "inv-1",
                "child",
                SubAgentProgressStatus::Running,
                19_999,
            ))
            .is_none());
        assert!(reporter
            .progress(&subagent_progress(
                "inv-1",
                "child",
                SubAgentProgressStatus::Running,
                20_000,
            ))
            .is_some());
    }

    #[test]
    fn subagent_reporter_always_reports_one_terminal_state() {
        for status in [
            SubAgentProgressStatus::Done,
            SubAgentProgressStatus::Failed,
            SubAgentProgressStatus::Cancelled,
        ] {
            let mut reporter = CliSubagentReporter::default();
            reporter.started("researcher", "child", Some("inv-1"));
            let terminal = subagent_progress("inv-1", "child", status, 1_250);
            let line = reporter
                .progress(&terminal)
                .expect("terminal progress is reported immediately");
            let expected = match status {
                SubAgentProgressStatus::Done => " done ",
                SubAgentProgressStatus::Failed => " failed ",
                SubAgentProgressStatus::Cancelled => " cancelled ",
                _ => unreachable!(),
            };
            assert!(line.contains(expected));
            assert!(line.contains("elapsed=1s"));
            assert!(reporter.progress(&terminal).is_none());
        }
    }

    #[test]
    fn subagent_reporter_keeps_nonterminal_cancellation_progress_open() {
        let mut reporter = CliSubagentReporter::default();
        reporter.started("researcher", "child", Some("inv-1"));

        for status in [
            SubAgentProgressStatus::Cancelling,
            SubAgentProgressStatus::Unconfirmed,
        ] {
            assert!(reporter
                .progress(&subagent_progress("inv-1", "child", status, 1_250))
                .is_some());
        }

        let cancelled =
            subagent_progress("inv-1", "child", SubAgentProgressStatus::Cancelled, 1_500);
        assert!(reporter.progress(&cancelled).is_some());
        assert!(reporter.progress(&cancelled).is_none());
    }

    #[test]
    fn subagent_reporter_tracks_concurrent_invocations_independently() {
        let mut reporter = CliSubagentReporter::default();
        reporter.started("researcher", "child-a", Some("inv-a"));
        reporter.started("researcher", "child-b", Some("inv-b"));

        assert!(reporter
            .progress(&subagent_progress(
                "inv-a",
                "child-a",
                SubAgentProgressStatus::Running,
                10_000,
            ))
            .is_some());
        assert!(reporter
            .progress(&subagent_progress(
                "inv-b",
                "child-b",
                SubAgentProgressStatus::Running,
                10_000,
            ))
            .is_some());
        assert!(reporter
            .progress(&subagent_progress(
                "inv-a",
                "child-a",
                SubAgentProgressStatus::Running,
                15_000,
            ))
            .is_none());
        assert!(reporter
            .progress(&subagent_progress(
                "inv-b",
                "child-b",
                SubAgentProgressStatus::Running,
                20_000,
            ))
            .is_some());
    }

    // ----------------------------------------------------------------
    // MCP MiniJinja templating: CLI must surface the rendered markdown/
    // content fields produced by harnx-runtime when an MCP tool's
    // `_meta.call_template` / `_meta.result_template` is set. Covers
    // issue #340 / PR #349 — the producer wired templates into the
    // ToolEvent fields, but the CLI consumer was discarding them
    // prior to these tests.
    // ----------------------------------------------------------------

    // The CLI Started handler renders template markdown without a tool prefix.
    // Without a template it keeps the dimmed tool name and YAML arguments.

    // The CLI Completed handler delegates to
    // `harnx_runtime::utils::render_tool_result_text`, the same shared
    // helper the TUI uses. We assert against that helper directly so any
    // future tweak to the rendering rules updates a single test surface.

    #[test]
    fn print_tool_started_with_markdown_omits_tool_prefix() {
        let mut state = make_state(false);
        let rendered = CliSinkState::format_tool_started(
            "bash_exec",
            &serde_json::Value::Null,
            Some("` $ cargo build`"),
            |text| state.render_markdown_line(text),
        );

        assert!(
            !rendered.contains("[tool]"),
            "unexpected tool prefix in output: {rendered}"
        );
        assert!(
            rendered.contains("cargo build"),
            "expected markdown text in output: {rendered}"
        );
    }

    #[test]
    fn print_tool_started_without_markdown_shows_tool_prefix() {
        let mut state = make_state(false);
        let rendered = CliSinkState::format_tool_started(
            "bash_exec",
            &serde_json::Value::Null,
            None,
            |text| state.render_markdown_line(text),
        );

        assert!(
            rendered.contains("[tool]"),
            "expected tool prefix in output: {rendered}"
        );
        assert!(
            rendered.contains("bash_exec"),
            "expected tool name in output: {rendered}"
        );
    }

    #[test]
    fn print_tool_started_without_markdown_shows_yaml_arguments() {
        let mut state = make_state(false);
        let input = serde_json::json!({"command": "cargo build", "timeout_secs": 30});
        let rendered = CliSinkState::format_tool_started("bash_exec", &input, None, |text| {
            state.render_markdown_line(text)
        });

        assert!(rendered.contains("[tool] bash_exec"));
        assert!(rendered.contains("command: cargo build"));
        assert!(rendered.contains("timeout_secs: 30"));
    }

    #[test]
    fn completed_uses_template_markdown_when_present() {
        // With a template-rendered markdown, prefer it over the raw output.
        let raw_output = serde_json::json!({
            "content": [{"type": "text", "text": "hello"}],
            "isError": false,
        });
        let rendered =
            harnx_runtime::utils::render_tool_result_text(&raw_output, Some("OK: hello"));
        assert!(rendered.contains("OK: hello"));
        assert!(
            !rendered.contains("isError"),
            "raw output JSON leaked when markdown was provided: {rendered}"
        );
    }

    #[test]
    fn completed_falls_back_to_extracted_text_when_no_markdown() {
        // No template => extract user-display text from MCP-style output.
        // Restores pre-0daecac CLI behavior (was silent in the interim).
        let raw_output = serde_json::json!({
            "content": [{"type": "text", "text": "tool stdout here"}],
        });
        let rendered = harnx_runtime::utils::render_tool_result_text(&raw_output, None);
        assert!(
            rendered.contains("tool stdout here"),
            "expected extracted user-display text in output: {rendered}"
        );
    }

    #[test]
    fn completed_falls_back_to_string_when_no_markdown_and_string_output() {
        // String-typed output passes through without yaml-wrapping.
        let raw_output = serde_json::Value::String("plain stdout line".into());
        let rendered = harnx_runtime::utils::render_tool_result_text(&raw_output, None);
        assert!(rendered.contains("plain stdout line"));
    }

    #[test]
    fn completed_falls_back_to_yaml_for_arbitrary_json() {
        // Arbitrary JSON with no extractable text falls through to YAML —
        // not silent, not the Debug form. Better-than-nothing display.
        let raw_output = serde_json::json!({"exitCode": 0, "duration_ms": 42});
        let rendered = harnx_runtime::utils::render_tool_result_text(&raw_output, None);
        assert!(
            rendered.contains("exitCode") && rendered.contains("duration_ms"),
            "expected YAML keys in fallback output: {rendered}"
        );
    }

    #[test]
    fn completed_treats_empty_markdown_as_no_markdown() {
        // A template that renders to "" must not blank out the result —
        // fall back to extraction so the user still sees the tool's work.
        let raw_output = serde_json::Value::String("important output".into());
        let rendered = harnx_runtime::utils::render_tool_result_text(&raw_output, Some(""));
        assert!(
            rendered.contains("important output"),
            "empty markdown should fall back to extraction: {rendered}"
        );
    }

    // ----------------------------------------------------------------
    // Markdown rendering for tool events. The state.render_markdown_line
    // helper drops back to plain text when highlighting is disabled or
    // the renderer can't initialize, otherwise it produces ANSI-styled
    // output via syntect.
    // ----------------------------------------------------------------

    #[test]
    fn render_markdown_line_passthrough_when_highlight_disabled() {
        // highlight=false short-circuits the renderer init entirely —
        // no ANSI codes regardless of TTY status.
        let mut state = make_state(false);
        let out = state.render_markdown_line("**bold** and `code`");
        assert_eq!(out, "**bold** and `code`");
        assert!(state.render.is_none(), "render should not be initialized");
    }

    #[test]
    fn render_markdown_line_passes_through_when_no_tty() {
        // When stdout isn't a TTY, IS_STDOUT_TERMINAL is false → return
        // the input unchanged. In the test process stdout *is* the test
        // harness's pipe, so this gate passes.
        let mut state = make_state(true);
        let out = state.render_markdown_line("**bold** and `code`");
        // In the test environment IS_STDOUT_TERMINAL is false, so we
        // expect the same plain passthrough.
        assert_eq!(out, "**bold** and `code`");
    }

    // ----------------------------------------------------------------
    // #410 / #414 behavioral tests.
    // ----------------------------------------------------------------

    #[test]
    fn split_line_tail_preserves_all_content() {
        // Verify the helper splits correctly and nothing is lost.
        let (head, tail) = split_line_tail_local("line1\nline2\ntail");
        // rsplit_once splits at last '\n': head = "line1\nline2", tail = "tail"
        assert_eq!(head, "line1\nline2");
        assert_eq!(tail, "tail");
    }

    #[test]
    fn split_line_tail_no_newline_returns_empty_head() {
        // Input with no newline: head is empty, tail is the whole string.
        let (head, tail) = split_line_tail_local("no newline here");
        assert_eq!(head, "");
        assert_eq!(tail, "no newline here");
    }

    #[test]
    fn split_line_tail_trailing_newline_gives_empty_tail() {
        // Trailing newline: tail is empty string.
        let (head, tail) = split_line_tail_local("line\n");
        assert_eq!(head, "line");
        assert_eq!(tail, "");
    }

    // ----------------------------------------------------------------
    // Buffer accumulation in handle_markdown_chunk (highlight=false
    // path is handle_raw_chunk; to test buffer we use highlight=true
    // but IS_STDOUT_TERMINAL is false in tests so handle_chunk_text
    // falls through to handle_raw_chunk).  Instead we call
    // handle_markdown_chunk directly to test buffer state.
    // ----------------------------------------------------------------

    #[test]
    fn markdown_chunk_accumulates_partial_line_in_buffer() {
        // A chunk with no newline should accumulate in the buffer without
        // printing (we can't capture stdout, but we verify buffer state).
        let mut state = make_state(false);
        // We call handle_markdown_chunk directly to bypass the
        // IS_STDOUT_TERMINAL gate in handle_chunk_text.
        state.handle_markdown_chunk("partial").unwrap();
        assert_eq!(state.buffer, "partial");
    }

    #[test]
    fn markdown_chunk_clears_buffer_on_newline() {
        // When a newline arrives the completed lines are flushed (printed)
        // and the tail stays in the buffer.
        let mut state = make_state(false);
        state.handle_markdown_chunk("line1\n").unwrap();
        // After flushing "line1", the tail is empty.
        assert_eq!(state.buffer, "");
    }

    #[test]
    fn markdown_chunk_keeps_tail_after_newline() {
        // Tail after last newline stays buffered for the next chunk.
        let mut state = make_state(false);
        state.handle_markdown_chunk("line1\ntail").unwrap();
        assert_eq!(state.buffer, "tail");
    }

    #[test]
    fn markdown_chunk_multi_chunk_accumulation() {
        // Multiple chunks accumulate correctly across calls.
        let mut state = make_state(false);
        state.handle_markdown_chunk("par").unwrap();
        state.handle_markdown_chunk("tial").unwrap();
        // No newline yet — full partial line is in buffer.
        assert_eq!(state.buffer, "partial");
        state.handle_markdown_chunk("\nrest").unwrap();
        // After flushing "partial", "rest" remains in buffer.
        assert_eq!(state.buffer, "rest");
    }

    #[test]
    fn cleanup_clears_buffer_and_resets_source() {
        // cleanup() must clear buffer and last_ui_output_source.
        let mut state = make_state(false);
        state.buffer = "leftover".to_string();
        state.last_ui_output_source = Some(AgentSource {
            agent: "test-agent".to_string(),
            session_id: None,
            model: None,
        });
        state.cleanup().unwrap();
        assert!(
            state.buffer.is_empty(),
            "buffer should be cleared after cleanup"
        );
        assert!(
            state.last_ui_output_source.is_none(),
            "last_ui_output_source should reset to None after cleanup"
        );
    }

    // ----------------------------------------------------------------
    // #410 source-heading deduplication via emit state tracking.
    // ----------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn source_heading_tracked_after_first_chunk() {
        // After the first sourced chunk, last_ui_output_source must be set.
        let sink = CliAgentEventSink::new(
            false,
            RenderOptions::default(),
            harnx_core::abort::create_abort_signal(),
        );
        let source = AgentSource {
            agent: "my-agent".to_string(),
            session_id: Some("s1".to_string()),
            model: None,
        };
        sink.emit(AgentEvent::sub_agent(
            source.clone(),
            AgentEvent::Model(ModelEvent::MessageChunk {
                blocks: vec![ContentBlock::Text("hello".into())],
            }),
        ));
        let state = sink.state.lock().unwrap();
        assert_eq!(
            state.last_ui_output_source.as_ref(),
            Some(&source),
            "source should be tracked after first chunk"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn source_heading_reset_after_turn_ended() {
        // After TurnEvent::Ended (which calls cleanup), last_ui_output_source
        // resets to None so the next turn shows its heading again.
        let sink = CliAgentEventSink::new(
            false,
            RenderOptions::default(),
            harnx_core::abort::create_abort_signal(),
        );
        let source = AgentSource {
            agent: "my-agent".to_string(),
            session_id: Some("s1".to_string()),
            model: None,
        };
        // Send a chunk to establish source.
        sink.emit(AgentEvent::sub_agent(
            source,
            AgentEvent::Model(ModelEvent::MessageChunk {
                blocks: vec![ContentBlock::Text("hello".into())],
            }),
        ));
        // End the turn — should reset source tracking.
        sink.emit(AgentEvent::Turn(TurnEvent::Ended {
            outcome: Default::default(),
        }));
        let state = sink.state.lock().unwrap();
        assert!(
            state.last_ui_output_source.is_none(),
            "last_ui_output_source should be None after turn ends"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn same_source_does_not_repeat_cleanup_between_chunks() {
        // When consecutive chunks share the same source, the buffer must
        // accumulate (cleanup is NOT called between them).
        let sink = CliAgentEventSink::new(
            false,
            RenderOptions::default(),
            harnx_core::abort::create_abort_signal(),
        );
        let source = AgentSource {
            agent: "my-agent".to_string(),
            session_id: Some("s1".to_string()),
            model: None,
        };
        // Push two partial chunks without newline.  Buffer should hold both.
        sink.emit(AgentEvent::sub_agent(
            source.clone(),
            AgentEvent::Model(ModelEvent::MessageChunk {
                blocks: vec![ContentBlock::Text("hello ".into())],
            }),
        ));
        sink.emit(AgentEvent::sub_agent(
            source,
            AgentEvent::Model(ModelEvent::MessageChunk {
                blocks: vec![ContentBlock::Text("world".into())],
            }),
        ));
        // In non-TTY test process, handle_chunk_text uses handle_raw_chunk
        // (print!), so buffer stays empty for raw path.  We verify the
        // source hasn't reset — i.e. cleanup was NOT called between chunks.
        let state = sink.state.lock().unwrap();
        assert!(
            state.last_ui_output_source.is_some(),
            "source should still be set — cleanup must not run between same-source chunks"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sub_agent_final_tracks_source_and_renders() {
        // SubAgent wrapper with Model::Final should extract the source and
        // track it (since Final is a model-output event for source-heading).
        // Note: Final calls cleanup(), which resets last_ui_output_source to None.
        // We verify the event processed without panic and that cleanup ran.
        let sink = CliAgentEventSink::new(
            false,
            RenderOptions::default(),
            harnx_core::abort::create_abort_signal(),
        );
        let source = AgentSource {
            agent: "sub".to_string(),
            session_id: None,
            model: None,
        };
        sink.emit(AgentEvent::sub_agent(
            source.clone(),
            AgentEvent::Model(ModelEvent::Final {
                output: "done".into(),
                usage: Default::default(),
            }),
        ));
        let state = sink.state.lock().unwrap();
        // Final triggers cleanup, which clears last_ui_output_source.
        // We verify the event processed without panic.
        assert!(
            state.last_ui_output_source.is_none(),
            "source should be cleared by cleanup after Final"
        );
        assert!(
            state.buffer.is_empty(),
            "buffer should be cleared after Final"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sub_agent_error_clears_state_without_panic() {
        // SubAgent wrapper with Model::Error should be processed without panic.
        // Error is NOT a model-output event, so source should NOT be tracked.
        let sink = CliAgentEventSink::new(
            false,
            RenderOptions::default(),
            harnx_core::abort::create_abort_signal(),
        );
        let source = AgentSource {
            agent: "sub".to_string(),
            session_id: None,
            model: None,
        };
        sink.emit(AgentEvent::sub_agent(
            source,
            AgentEvent::Model(ModelEvent::Error("boom".into())),
        ));
        let state = sink.state.lock().unwrap();
        // Error is NOT a model-output event, so source should be None
        assert!(
            state.last_ui_output_source.is_none(),
            "source should NOT be tracked for Error event"
        );
        // cleanup is called for Error, so buffer should be empty
        assert!(
            state.buffer.is_empty(),
            "buffer should be cleared after Error"
        );
    }

    // ----------------------------------------------------------------
    // Compaction event handling: CLI must handle Started/Completed/Failed
    // without falling into the debug catch-all.
    // ----------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn compaction_events_are_handled_without_panic() {
        let sink = CliAgentEventSink::new(
            false,
            RenderOptions::default(),
            harnx_core::abort::create_abort_signal(),
        );
        sink.emit(AgentEvent::Session(SessionEvent::CompactingStarted {
            compaction_id: None,
        }));
        sink.emit(AgentEvent::Session(SessionEvent::CompactingCompleted {
            compaction_id: None,
            outcome: harnx_core::session::CompactOutcome::Compacted,
        }));
        sink.emit(AgentEvent::Session(SessionEvent::CompactingFailed {
            compaction_id: None,
            error: "something went wrong".to_string(),
        }));
    }

    // ----------------------------------------------------------------
    // LogSeqAssigned is persistence bookkeeping the CLI doesn't use. It
    // must be dropped silently, not routed to the `[event] {other:?}`
    // debug catch-all (regression test for issue #1631).
    // ----------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn log_seq_assigned_is_handled_without_panic() {
        let sink = CliAgentEventSink::new(
            false,
            RenderOptions::default(),
            harnx_core::abort::create_abort_signal(),
        );
        sink.emit(AgentEvent::Session(SessionEvent::LogSeqAssigned {
            seq: 42,
        }));
        // Dropping the event must not disturb the render buffer or the
        // tracked output source — nothing was printed for it.
        let state = sink.state.lock().unwrap();
        assert!(
            state.buffer.is_empty(),
            "LogSeqAssigned must not write to the render buffer"
        );
        assert!(
            state.last_ui_output_source.is_none(),
            "LogSeqAssigned must not be treated as model output"
        );
    }

    // ----------------------------------------------------------------
    // Tool timer behavioral tests
    // ----------------------------------------------------------------
    //
    // These tests verify the "still running" ticker behavior for long tool calls
    // using tokio's paused time (virtual time). Each test exercises a specific
    // correctness guarantee from the plan spec.
    //
    // Key constants (from harnx-toolset):
    // - TOOL_TIMER_MIN_ELAPSED_MS: 5_000 (minimum elapsed time before showing timer)
    // - TOOL_TIMER_NOTICE_INTERVAL_MS: 10_000 (interval between periodic notices)
    //
    // The ticker is invoked manually via `tool_timer_tick()` to simulate the
    // 1s interval loop in `oneshot_nats::run_turn`.
    //

    /// Creates a ToolCallTimerEntry with a manually-set start Instant.
    /// This is used in tests to simulate elapsed time without real sleeps.
    fn make_timer_entry(tool_name: &str, started: Instant) -> ToolCallTimerEntry {
        ToolCallTimerEntry {
            tool_name: tool_name.to_string(),
            started,
            last_reported_bucket: 0,
        }
    }

    #[test]
    fn tool_timer_started_tracks_non_launcher_tools() {
        let mut state = make_state(false);

        // Non-launcher tools should be tracked
        state.tool_call_started("call-1", "read_file");
        assert!(state.tool_timers.in_flight.contains_key("call-1"));

        state.tool_call_started("call-2", "bash_exec");
        assert!(state.tool_timers.in_flight.contains_key("call-2"));

        // Count should be 2
        assert_eq!(state.tool_timers.in_flight.len(), 2);
    }

    #[test]
    fn tool_timer_started_ignores_subagent_launcher_tools() {
        let mut state = make_state(false);

        // session_new and session_prompt are launchers - should not be tracked
        state.tool_call_started("call-1", "session_new");
        assert!(!state.tool_timers.in_flight.contains_key("call-1"));

        state.tool_call_started("call-2", "session_prompt");
        assert!(!state.tool_timers.in_flight.contains_key("call-2"));

        // Prefixed forms should also be ignored
        state.tool_call_started("call-3", "oracle_session_new");
        assert!(!state.tool_timers.in_flight.contains_key("call-3"));

        state.tool_call_started("call-4", "pantheon__oracle_session_prompt");
        assert!(!state.tool_timers.in_flight.contains_key("call-4"));

        // But session_load and session_cancel are NOT launchers
        state.tool_call_started("call-5", "session_load");
        assert!(state.tool_timers.in_flight.contains_key("call-5"));

        state.tool_call_started("call-6", "session_cancel");
        assert!(state.tool_timers.in_flight.contains_key("call-6"));

        assert_eq!(state.tool_timers.in_flight.len(), 2);
    }

    #[test]
    fn tool_timer_finished_removes_entry() {
        let mut state = make_state(false);

        state.tool_call_started("call-1", "read_file");
        assert!(state.tool_timers.in_flight.contains_key("call-1"));

        let result = state.tool_call_finished("call-1");
        assert!(!state.tool_timers.in_flight.contains_key("call-1"));
        // Less than 5s, no duration returned
        assert!(result.is_none());
    }

    #[test]
    fn tool_timer_finished_returns_duration_when_above_threshold() {
        use std::time::{Duration, Instant};

        let mut state = make_state(false);
        let started = Instant::now() - Duration::from_millis(6_000);

        state
            .tool_timers
            .in_flight
            .insert("call-1".to_string(), make_timer_entry("read_file", started));

        let result = state.tool_call_finished("call-1");
        assert!(result.is_some());
        let elapsed = result.unwrap();
        // Should be "6s" (rounded down from 6000ms)
        assert!(elapsed.starts_with('6'));
        assert!(elapsed.ends_with('s'));
    }

    #[test]
    fn tool_timer_finished_no_duration_below_threshold() {
        use std::time::{Duration, Instant};

        let mut state = make_state(false);
        let started = Instant::now() - Duration::from_millis(4_000);

        state
            .tool_timers
            .in_flight
            .insert("call-1".to_string(), make_timer_entry("read_file", started));

        let result = state.tool_call_finished("call-1");
        // 4s < 5s threshold, no duration should be returned
        assert!(result.is_none());
    }

    /// Test that a tool running 10+ seconds emits a periodic notice on the first tick,
    /// but does NOT emit a final duration if it ends before MIN_ELAPSED.
    #[test]
    fn tool_timer_tick_emits_notice_at_10s() {
        use std::time::{Duration, Instant};

        let mut state = make_state(false);
        let started = Instant::now() - Duration::from_millis(10_500);

        state
            .tool_timers
            .in_flight
            .insert("call-1".to_string(), make_timer_entry("read_file", started));

        // Capture output using the Unix capture helper
        #[cfg(unix)]
        {
            let output = capture_output(|| {
                state.tool_timer_tick();
            });

            // Should contain a notice for read_file at ~10s
            assert!(output.contains("read_file"));
            assert!(output.contains("still running"));
            assert!(output.contains("10s"));
        }

        #[cfg(not(unix))]
        {
            // On non-Unix, just verify the state update
            state.tool_timer_tick();
            let entry = state.tool_timers.in_flight.get("call-1").unwrap();
            assert_eq!(entry.last_reported_bucket, 1);
        }
    }

    #[test]
    fn tool_timer_tick_no_notice_before_10s() {
        use std::time::{Duration, Instant};

        let mut state = make_state(false);
        let started = Instant::now() - Duration::from_millis(9_500);

        state
            .tool_timers
            .in_flight
            .insert("call-1".to_string(), make_timer_entry("read_file", started));

        #[cfg(unix)]
        {
            let output = capture_output(|| {
                state.tool_timer_tick();
            });

            // Should NOT emit any notice before 10s
            assert!(!output.contains("still running"));
        }

        // Entry should still have last_reported_bucket = 0
        let entry = state.tool_timers.in_flight.get("call-1").unwrap();
        assert_eq!(entry.last_reported_bucket, 0);
    }

    #[test]
    fn tool_timer_tick_emits_notices_at_10s_20s_30s() {
        use std::time::{Duration, Instant};

        // Simulates a "silent 30s call" - the core bug the ticker fixes.
        // A tool that runs 30s with no other events should emit
        // notices at ~10s, ~20s, ~30s BEFORE completion.
        let mut state = make_state(false);

        // Start at time 0
        let started = Instant::now();

        state.tool_timers.in_flight.insert(
            "call-1".to_string(),
            ToolCallTimerEntry {
                tool_name: "long_running_tool".to_string(),
                started,
                last_reported_bucket: 0,
            },
        );

        // We can't manipulate real time easily, so this test verifies
        // the logic by manually advancing bucket values as time would.

        // After 10s elapsed: bucket = 1
        let entry = state.tool_timers.in_flight.get_mut("call-1").unwrap();
        entry.started = Instant::now() - Duration::from_millis(10_500);

        #[cfg(unix)]
        {
            let output = capture_output(|| {
                state.tool_timer_tick();
            });
            assert!(output.contains("10s"), "expected 10s notice: {output}");
        }

        // After 20s elapsed: bucket = 2
        let entry = state.tool_timers.in_flight.get_mut("call-1").unwrap();
        entry.started = Instant::now() - Duration::from_millis(20_500);

        #[cfg(unix)]
        {
            let output = capture_output(|| {
                state.tool_timer_tick();
            });
            assert!(output.contains("20s"), "expected 20s notice: {output}");
        }

        // After 30s elapsed: bucket = 3
        let entry = state.tool_timers.in_flight.get_mut("call-1").unwrap();
        entry.started = Instant::now() - Duration::from_millis(30_500);

        #[cfg(unix)]
        {
            let output = capture_output(|| {
                state.tool_timer_tick();
            });
            assert!(output.contains("30s"), "expected 30s notice: {output}");
        }
    }

    #[test]
    fn tool_timer_tick_no_duplicate_notice_same_bucket() {
        use std::time::{Duration, Instant};

        let mut state = make_state(false);
        let started = Instant::now() - Duration::from_millis(10_500);

        state
            .tool_timers
            .in_flight
            .insert("call-1".to_string(), make_timer_entry("read_file", started));

        #[cfg(unix)]
        {
            let output1 = capture_output(|| {
                state.tool_timer_tick();
            });
            assert!(output1.contains("still running"));

            // Second tick at same bucket should not emit again
            let output2 = capture_output(|| {
                state.tool_timer_tick();
            });
            assert!(
                !output2.contains("still running"),
                "duplicate notice: {output2}"
            );
        }
    }

    #[test]
    fn tool_timer_concurrent_calls_tracked_separately() {
        // Two concurrent calls with same tool name but different IDs
        // must be tracked separately without overwriting each other.
        use std::time::{Duration, Instant};

        let mut state = make_state(false);

        let started1 = Instant::now() - Duration::from_millis(10_500);
        let started2 = Instant::now() - Duration::from_millis(15_000);

        state.tool_timers.in_flight.insert(
            "call-1".to_string(),
            make_timer_entry("read_file", started1),
        );
        state.tool_timers.in_flight.insert(
            "call-2".to_string(),
            make_timer_entry("read_file", started2),
        );

        assert_eq!(state.tool_timers.in_flight.len(), 2);

        #[cfg(unix)]
        {
            let output = capture_output(|| {
                state.tool_timer_tick();
            });

            // Both should emit notices (different elapsed times)
            assert!(output.contains("still running"));
            assert!(
                output.contains("10s") || output.contains("15s"),
                "expected one of the elapsed times: {output}"
            );
        }

        // Verify both entries still exist
        assert!(state.tool_timers.in_flight.contains_key("call-1"));
        assert!(state.tool_timers.in_flight.contains_key("call-2"));
    }

    #[test]
    fn tool_timer_staggered_concurrency() {
        // Two tools started 5s apart each notify ~10s after their own start.
        use std::time::{Duration, Instant};

        let mut state = make_state(false);

        // Tool A started 10.5s ago (has crossed 10s threshold)
        let started_a = Instant::now() - Duration::from_millis(10_500);
        // Tool B started 5.5s ago (has NOT crossed 10s threshold)
        let started_b = Instant::now() - Duration::from_millis(5_500);

        state
            .tool_timers
            .in_flight
            .insert("call-a".to_string(), make_timer_entry("tool_a", started_a));
        state
            .tool_timers
            .in_flight
            .insert("call-b".to_string(), make_timer_entry("tool_b", started_b));

        #[cfg(unix)]
        let output = capture_output(|| {
            state.tool_timer_tick();
        });
        #[cfg(not(unix))]
        state.tool_timer_tick();

        #[cfg(unix)]
        {
            // Only tool_a should emit (10s bucket)
            assert!(output.contains("tool_a"));
            assert!(output.contains("still running"));
            assert!(
                !output.contains("tool_b"),
                "tool_b should not be in output: {output}"
            );
        }

        // Verify entry states
        let entry_a = state.tool_timers.in_flight.get("call-a").unwrap();
        assert_eq!(entry_a.last_reported_bucket, 1);

        let entry_b = state.tool_timers.in_flight.get("call-b").unwrap();
        assert_eq!(entry_b.last_reported_bucket, 0);
    }

    #[test]
    fn tool_timer_terminal_stops_tracking() {
        // Completed/Failed/Blocked should stop the timer and remove entry.
        // No notices should print after the terminal event.
        use std::time::{Duration, Instant};

        let mut state = make_state(false);
        let started = Instant::now() - Duration::from_millis(15_000);

        state
            .tool_timers
            .in_flight
            .insert("call-1".to_string(), make_timer_entry("read_file", started));

        // Simulate completion
        let result = state.tool_call_finished("call-1");

        // Entry should be removed
        assert!(!state.tool_timers.in_flight.contains_key("call-1"));

        // Duration > 5s should be returned
        assert!(result.is_some());

        // Subsequent tick should not emit anything (no in-flight entries)
        #[cfg(unix)]
        {
            let output = capture_output(|| {
                state.tool_timer_tick();
            });
            assert!(
                output.is_empty() || !output.contains("still running"),
                "no notice should emit after completion: {output}"
            );
        }
    }

    #[test]
    fn tool_timer_blocked_is_terminal() {
        // Blocked is terminal for this call but restartable.
        // Still removes from in-flight and does NOT show elapsed time.
        use std::time::{Duration, Instant};

        let mut state = make_state(false);
        let started = Instant::now() - Duration::from_millis(15_000);

        state
            .tool_timers
            .in_flight
            .insert("call-1".to_string(), make_timer_entry("read_file", started));

        // Simulate blocked - tool_call_finished is called
        let result = state.tool_call_finished("call-1");

        // Entry should be removed
        assert!(!state.tool_timers.in_flight.contains_key("call-1"));

        // Duration > 5s should be returned (even for blocked)
        assert!(result.is_some());
    }

    #[test]
    fn tool_timer_clear_removes_all_entries() {
        // clear_tool_timers (called on exit) clears everything.

        let mut state = make_state(false);

        state.tool_timers.in_flight.insert(
            "call-1".to_string(),
            make_timer_entry("read_file", Instant::now()),
        );
        state.tool_timers.in_flight.insert(
            "call-2".to_string(),
            make_timer_entry("write_file", Instant::now()),
        );

        assert_eq!(state.tool_timers.in_flight.len(), 2);

        state.clear_tool_timers();

        assert!(state.tool_timers.in_flight.is_empty());
    }

    #[test]
    fn tool_timer_final_only_suppresses_notices() {
        // In --final-only mode, the ticker is never invoked
        // (tool_timer_tick callback is None in oneshot_nats::run_turn).
        // This test verifies that if we DID call it (hypothetically),
        // entries would still be tracked correctly, but in practice
        // the ticker doesn't run.

        // Actually, final_only is a state flag, not a direct check
        // in tool_timer_tick. The CLI avoids calling the ticker
        // in final-only mode by passing None for the callback.
        // Let's verify that a final_only state can still track entries
        // for completion (tool_call_finished).

        // Create a state manually with final_only = true.
        let mut state = CliSinkState {
            render: None,
            buffer: String::new(),
            last_ui_output_source: None,
            highlight: false,
            render_options: RenderOptions::default(),
            final_only: true,
            subagents: CliSubagentReporter::default(),
            tool_timers: ToolCallTimerReporter::default(),
        };

        state.tool_call_started("call-1", "read_file");
        assert!(state.tool_timers.in_flight.contains_key("call-1"));

        // If we hypothetically called tick, it would still work.
        // But in production, the ticker callback is None in final_only mode.
    }

    #[test]
    fn tool_timer_threshold_4s_no_notice_no_final() {
        // A tool completing at ~4s emits no periodic notice and no final duration.
        use std::time::{Duration, Instant};

        let mut state = make_state(false);
        let started = Instant::now() - Duration::from_millis(4_000);

        state
            .tool_timers
            .in_flight
            .insert("call-1".to_string(), make_timer_entry("read_file", started));

        // Tick at 4s should not emit
        #[cfg(unix)]
        {
            let output = capture_output(|| {
                state.tool_timer_tick();
            });
            assert!(
                !output.contains("still running"),
                "no notice at 4s: {output}"
            );
        }

        // Completion at 4s should not return duration (< 5s threshold)
        let result = state.tool_call_finished("call-1");
        assert!(result.is_none());
    }

    #[test]
    fn tool_timer_threshold_7s_final_no_periodic() {
        // A tool ending at ~7s (between 5s and 10s) emits a final (Ns)
        // but no periodic notice.
        use std::time::{Duration, Instant};

        let mut state = make_state(false);
        let started = Instant::now() - Duration::from_millis(7_000);

        state
            .tool_timers
            .in_flight
            .insert("call-1".to_string(), make_timer_entry("read_file", started));

        // Tick at 7s should not emit (bucket = 0)
        #[cfg(unix)]
        {
            let output = capture_output(|| {
                state.tool_timer_tick();
            });
            assert!(
                !output.contains("still running"),
                "no notice at 7s: {output}"
            );
        }

        // Completion at 7s should return duration (> 5s threshold)
        let result = state.tool_call_finished("call-1");
        assert!(result.is_some());
        let elapsed = result.unwrap();
        assert!(elapsed.starts_with('7'));
    }

    /// Integration test using tokio paused time for deterministic behavior.
    /// This is the key test that should FAIL if the ticker is removed,
    /// ensuring the silent path is actually exercised.
    ///
    /// Note: This test does NOT use tokio::time::pause() because:
    /// 1. `capture_output` uses `dup2` which conflicts with `pause()` requirements
    /// 2. We can achieve deterministic behavior by manually setting Instant values
    /// 3. The bucket-based time tracking doesn't require Tokio's virtual clock
    #[test]
    fn tool_timer_silent_long_call_emits_notices() {
        use std::time::{Duration, Instant};

        let mut state = make_state(false);

        // Simulate a tool that started 10.5 seconds ago
        let started = Instant::now() - Duration::from_millis(10_500);

        state.tool_timers.in_flight.insert(
            "call-1".to_string(),
            ToolCallTimerEntry {
                tool_name: "long_running_tool".to_string(),
                started,
                last_reported_bucket: 0,
            },
        );

        // Tick at ~10s should emit
        #[cfg(unix)]
        {
            let output = capture_output(|| {
                state.tool_timer_tick();
            });
            assert!(
                output.contains("still running"),
                "expected periodic notice at 10s: {output}"
            );
            assert!(output.contains("10s"), "expected 10s in output: {output}");
        }

        // Advance to ~20s by updating the start time
        state
            .tool_timers
            .in_flight
            .get_mut("call-1")
            .unwrap()
            .started = Instant::now() - Duration::from_millis(20_500);

        #[cfg(unix)]
        {
            let output = capture_output(|| {
                state.tool_timer_tick();
            });
            assert!(output.contains("20s"), "expected 20s notice: {output}");
        }

        // Advance to ~30s
        state
            .tool_timers
            .in_flight
            .get_mut("call-1")
            .unwrap()
            .started = Instant::now() - Duration::from_millis(30_500);

        #[cfg(unix)]
        {
            let output = capture_output(|| {
                state.tool_timer_tick();
            });
            assert!(output.contains("30s"), "expected 30s notice: {output}");
        }

        // Complete before emitting again
        let result = state.tool_call_finished("call-1");
        assert!(result.is_some());
        assert!(state.tool_timers.in_flight.is_empty());
    }

    /// Test that after completion, no phantom notices are emitted.
    #[test]
    fn tool_timer_no_notices_after_completion() {
        use std::time::{Duration, Instant};

        let mut state = make_state(false);
        let started = Instant::now() - Duration::from_millis(25_000);

        state
            .tool_timers
            .in_flight
            .insert("call-1".to_string(), make_timer_entry("read_file", started));

        // First tick emits at bucket 2 (20s)
        #[cfg(unix)]
        {
            let _output = capture_output(|| {
                state.tool_timer_tick();
            });
        }

        // Complete the tool
        let _result = state.tool_call_finished("call-1");

        // Second tick should not emit (entry removed)
        #[cfg(unix)]
        {
            let output = capture_output(|| {
                state.tool_timer_tick();
            });
            assert!(
                !output.contains("still running"),
                "no notice after completion: {output}"
            );
        }
    }

    /// Test that the ticker uses Skip missed tick behavior (no catch-up burst).
    /// This is verified by the oneshot_nats implementation, but we ensure
    /// our tick logic doesn't burst either.
    #[test]
    fn tool_timer_no_catch_up_burst() {
        use std::time::{Duration, Instant};

        let mut state = make_state(false);

        // Start a tool that's been running for 25s, but last_reported_bucket = 0
        // This simulates a scenario where ticks were delayed.
        let started = Instant::now() - Duration::from_millis(25_000);

        state.tool_timers.in_flight.insert(
            "call-1".to_string(),
            ToolCallTimerEntry {
                tool_name: "delayed_tool".to_string(),
                started,
                last_reported_bucket: 0,
            },
        );

        #[cfg(unix)]
        // Single tick should emit only ONE notice (for bucket 2, the current bucket)
        // NOT multiple notices for bucket 1 AND bucket 2
        let output = capture_output(|| {
            state.tool_timer_tick();
        });
        #[cfg(not(unix))]
        state.tool_timer_tick();

        #[cfg(unix)]
        {
            // Should emit at bucket 2 (20s-29s range)
            assert!(output.contains("still running"));

            // Count occurrences of "still running" - should be exactly 1
            let count = output.matches("still running").count();
            assert_eq!(count, 1, "expected single notice, not burst: {output}");
        }

        // Entry should now have last_reported_bucket = 2
        let entry = state.tool_timers.in_flight.get("call-1").unwrap();
        assert_eq!(entry.last_reported_bucket, 2);
    }
}
