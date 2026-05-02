//! `CliAgentEventSink` renders `AgentEvent`s for the non-interactive CLI.
//!
//! Streaming chunks (`Model::MessageChunk` / `Model::ThoughtChunk`) are
//! written directly to stdout with optional markdown rendering + raw-mode
//! cursor manipulation — the sink transplants the display logic
//! previously owned by `render::render_stream`. Non-streaming events
//! (notices, errors, usage, tool starts/failures) still go to stderr.

use std::io::{stdout, Write};
use std::sync::{Arc, Mutex};

use harnx_core::event::{
    AgentEvent, AgentEventSink, AgentSource, ContentBlock, ModelEvent, NoticeEvent, ToolEvent,
    TurnEvent,
};

use harnx_render::{MarkdownRender, RenderOptions};
use harnx_runtime::utils::{dimmed_text, spawn_spinner, warning_text, Spinner, IS_STDOUT_TERMINAL};

/// Stderr-bound sink for the non-interactive CLI. Thread-safe — interior
/// state is held behind an `Arc<Mutex<CliSinkState>>` so multiple clones
/// of the sink share the same spinner/render buffer.
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
    )
}

fn source_heading(source: &AgentSource) -> String {
    match &source.session_id {
        Some(session_id) if !session_id.is_empty() => {
            format!("> {} ▸ {}", source.agent, session_id)
        }
        _ => format!("> {}", source.agent),
    }
}

struct CliSinkState {
    spinner: Option<Spinner>,
    render: Option<MarkdownRender>,
    buffer: String,
    last_ui_output_source: Option<AgentSource>,
    highlight: bool,
    render_options: RenderOptions,
}

impl CliAgentEventSink {
    pub fn new(
        highlight: bool,
        render_options: RenderOptions,
        _abort_signal: harnx_core::abort::AbortSignal,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(CliSinkState {
                spinner: None,
                render: None,
                buffer: String::new(),
                last_ui_output_source: None,
                highlight,
                render_options,
            })),
        }
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
    /// Stops the spinner on first chunk.
    fn handle_chunk_text(&mut self, text: &str) -> anyhow::Result<()> {
        if let Some(spinner) = self.spinner.take() {
            spinner.stop();
        }

        if self.highlight && *IS_STDOUT_TERMINAL {
            self.handle_markdown_chunk(text)
        } else {
            self.handle_raw_chunk(text)
        }
    }

    fn handle_raw_chunk(&mut self, text: &str) -> anyhow::Result<()> {
        print!("{text}");
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

    /// End-of-turn cleanup: stop spinner, flush any buffered partial line,
    /// and reset state so the next turn starts fresh.
    fn cleanup(&mut self) -> anyhow::Result<()> {
        if let Some(spinner) = self.spinner.take() {
            spinner.stop();
        }
        // The partial line in self.buffer has already been printed raw
        // (handle_markdown_chunk prints each chunk immediately).  We just
        // need a trailing newline to close the line on the terminal.
        if !self.buffer.is_empty() {
            println!();
        }
        self.buffer.clear();
        self.render = None;
        self.last_ui_output_source = None;
        Ok(())
    }

    /// Stderr render for `ToolEvent::Started`: when the producer rendered an
    /// MCP `call_template` into `markdown`, print only the markdown-styled
    /// line (no `[tool] name` prefix). When no markdown is present, fall back
    /// to the dim `[tool] {name}` prefix.
    fn print_tool_started(&mut self, name: &str, markdown: Option<&str>) {
        let rendered = Self::format_tool_started(name, markdown, |text| {
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
        markdown: Option<&str>,
        mut render_markdown: impl FnMut(&str) -> String,
    ) -> String {
        match markdown.map(str::trim).filter(|t| !t.is_empty()) {
            Some(t) => render_markdown(t),
            None => dimmed_text(&format!("[tool] {name}")),
        }
    }

    /// Stderr render for `ToolEvent::Completed`. Always routes through the
    /// multi-line `MarkdownRender::render` so block-level constructs work
    /// — fenced code (e.g. the ```diff blocks emitted by harnx-mcp-fs /
    /// harnx-mcp-bash for history diffs) gets syntect highlighting,
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
}

impl AgentEventSink for CliAgentEventSink {
    fn emit(&self, event: AgentEvent, source: Option<harnx_core::event::AgentSource>) {
        let mut state = match self.state.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if is_model_output_event(&event) {
            state.maybe_emit_source_heading(source.as_ref());
        }
        match event {
            AgentEvent::Status(line) => match &state.spinner {
                Some(spinner) => {
                    let _ = spinner.set_message(line.text);
                }
                None => {
                    state.spinner = Some(spawn_spinner(&line.text));
                }
            },
            AgentEvent::Turn(TurnEvent::Started) => {
                if state.spinner.is_none() {
                    state.spinner = Some(spawn_spinner("Generating"));
                }
            }
            AgentEvent::Turn(TurnEvent::Ended { .. }) => {
                if let Err(err) = state.cleanup() {
                    eprintln!(
                        "{}",
                        warning_text(&format!("cli-sink cleanup failed: {err}"))
                    );
                }
            }
            AgentEvent::Turn(TurnEvent::RetryAttempt { attempt, reason }) => {
                eprintln!("{}", warning_text(&format!("retry #{attempt}: {reason}")));
            }
            AgentEvent::Turn(TurnEvent::ModelFallback { from, to }) => {
                eprintln!(
                    "{}",
                    warning_text(&format!("model fallback: {from} → {to}"))
                );
            }
            AgentEvent::Turn(TurnEvent::HandoffRequested { agent, .. }) => {
                eprintln!("{}", dimmed_text(&format!("handoff → {agent}")));
            }
            AgentEvent::Notice(NoticeEvent::Info(msg)) => {
                // Info notices go to stdout — they're user-facing output
                // (save confirmations, dry-run echo, progress messages) that
                // shell scripts may pipe/capture. Warnings + Errors keep
                // their stderr routing below.
                println!("{msg}");
            }
            AgentEvent::Notice(NoticeEvent::Warning(msg)) => {
                eprintln!("{}", warning_text(&msg));
            }
            AgentEvent::Notice(NoticeEvent::Error(msg)) => {
                eprintln!("{}", warning_text(&format!("error: {msg}")));
            }
            AgentEvent::Model(ModelEvent::MessageChunk { blocks }) => {
                for block in blocks {
                    if let ContentBlock::Text(text) = block {
                        if let Err(err) = state.handle_chunk_text(&text) {
                            eprintln!("{}", warning_text(&format!("render failed: {err}")));
                            break;
                        }
                    }
                }
            }
            AgentEvent::Model(ModelEvent::ThoughtChunk { blocks }) => {
                // Treat thought chunks identically to message chunks for now;
                // richer <think>…</think> framing is deferred.
                for block in blocks {
                    if let ContentBlock::Text(text) = block {
                        if let Err(err) = state.handle_chunk_text(&text) {
                            eprintln!("{}", warning_text(&format!("render failed: {err}")));
                            break;
                        }
                    }
                }
            }
            AgentEvent::Model(ModelEvent::Final { output, .. }) => {
                // If streaming produced no chunks (short-circuit path),
                // print the full output so the user still sees it.
                if !output.is_empty() {
                    eprintln!("{output}");
                }
                if let Err(err) = state.cleanup() {
                    eprintln!(
                        "{}",
                        warning_text(&format!("cli-sink cleanup failed: {err}"))
                    );
                }
            }
            AgentEvent::Model(ModelEvent::Error(err)) => {
                if let Err(cleanup_err) = state.cleanup() {
                    eprintln!(
                        "{}",
                        warning_text(&format!("cli-sink cleanup failed: {cleanup_err}"))
                    );
                }
                eprintln!("{}", warning_text(&format!("LLM error: {err}")));
            }
            AgentEvent::Model(ModelEvent::Usage {
                input,
                output,
                cached,
                session_label: _,
            }) => {
                if input > 0 || output > 0 || cached > 0 {
                    let cached_suffix = if cached > 0 {
                        format!(" (cached {cached})")
                    } else {
                        String::new()
                    };
                    eprintln!(
                        "{}",
                        dimmed_text(&format!("[tokens] in={input} out={output}{cached_suffix}"))
                    );
                }
            }
            AgentEvent::Tool(ToolEvent::Started { name, markdown, .. }) => {
                state.print_tool_started(&name, markdown.as_deref());
            }
            AgentEvent::Tool(ToolEvent::Failed { error, .. }) => {
                eprintln!("{}", warning_text(&format!("tool error: {error}")));
            }
            AgentEvent::Tool(ToolEvent::Completed {
                output, markdown, ..
            }) => {
                state.print_tool_completed(&output, markdown.as_deref());
            }
            // Silent for Progress / Update — they are streamed mid-call
            // updates that would clutter stderr.
            AgentEvent::Tool(_) => {}
            // Every other variant — Session, Status, Plan — still gets
            // captured so nothing silently disappears. These receive dedicated
            // renderers in a future plan.
            other => eprintln!("{}", dimmed_text(&format!("[event] {other:?}"))),
        }
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
    use harnx_core::event::{ContentBlock, StatusLine};

    fn make_state(highlight: bool) -> CliSinkState {
        CliSinkState {
            spinner: None,
            render: None,
            buffer: String::new(),
            last_ui_output_source: None,
            highlight,
            render_options: RenderOptions::default(),
        }
    }

    // The sink writes to stdout (Info notices, streaming chunks) and stderr
    // (Warning/Error notices, status lines), which is hard to capture in a
    // unit test without subprocess machinery. We verify here only that
    // `emit` doesn't panic for a representative sample of event variants —
    // the behavioral verification (events arrive in the right order) lives
    // in the integration test at `tests/engine_smoke.rs`.

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn emit_handles_each_top_level_variant_without_panic() {
        let sink = CliAgentEventSink::new(
            false,
            RenderOptions::default(),
            harnx_core::abort::create_abort_signal(),
        );

        sink.emit(AgentEvent::Turn(TurnEvent::Started), None);
        sink.emit(
            AgentEvent::Turn(TurnEvent::Ended {
                outcome: Default::default(),
            }),
            None,
        );
        sink.emit(AgentEvent::Notice(NoticeEvent::Info("info".into())), None);
        sink.emit(
            AgentEvent::Notice(NoticeEvent::Warning("warn".into())),
            None,
        );
        sink.emit(AgentEvent::Notice(NoticeEvent::Error("err".into())), None);
        sink.emit(
            AgentEvent::Model(ModelEvent::MessageChunk {
                blocks: vec![ContentBlock::Text("hello".into())],
            }),
            None,
        );
        sink.emit(
            AgentEvent::Model(ModelEvent::Final {
                output: "done".into(),
                usage: Default::default(),
            }),
            None,
        );
        sink.emit(AgentEvent::Model(ModelEvent::Error("boom".into())), None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn status_event_starts_spinner_without_panic() {
        let sink = CliAgentEventSink::new(
            false,
            RenderOptions::default(),
            harnx_core::abort::create_abort_signal(),
        );
        sink.emit(
            AgentEvent::Status(StatusLine {
                text: "[test-model] generating".into(),
            }),
            None,
        );
        sink.emit(AgentEvent::Turn(TurnEvent::Started), None);
        sink.emit(
            AgentEvent::Turn(TurnEvent::Ended {
                outcome: Default::default(),
            }),
            None,
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn status_event_updates_existing_spinner() {
        let sink = CliAgentEventSink::new(
            false,
            RenderOptions::default(),
            harnx_core::abort::create_abort_signal(),
        );
        sink.emit(AgentEvent::Turn(TurnEvent::Started), None);
        sink.emit(
            AgentEvent::Status(StatusLine {
                text: "[rich-label] generating".into(),
            }),
            None,
        );
        sink.emit(
            AgentEvent::Turn(TurnEvent::Ended {
                outcome: Default::default(),
            }),
            None,
        );
    }

    // ----------------------------------------------------------------
    // MCP MiniJinja templating: CLI must surface the rendered markdown/
    // content fields produced by harnx-runtime when an MCP tool's
    // `_meta.call_template` / `_meta.result_template` is set. Covers
    // issue #340 / PR #349 — the producer wired templates into the
    // ToolEvent fields, but the CLI consumer was discarding them
    // prior to these tests.
    // ----------------------------------------------------------------

    // The CLI Started handler renders the bare "[tool] name" prefix
    // dimmed and appends the markdown-rendered text (or nothing if no
    // markdown). The whitespace/empty fallback decision lives inline in the
    // emit handler, exercised end-to-end by
    // `emit_handles_each_top_level_variant_without_panic` plus the
    // markdown-line tests below.

    // The CLI Completed handler delegates to
    // `harnx_runtime::utils::render_tool_result_text`, the same shared
    // helper the TUI uses. We assert against that helper directly so any
    // future tweak to the rendering rules updates a single test surface.

    #[test]
    fn print_tool_started_with_markdown_omits_tool_prefix() {
        let mut state = make_state(false);
        let rendered =
            CliSinkState::format_tool_started("bash_exec", Some("` $ cargo build`"), |text| {
                state.render_markdown_line(text)
            });

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
        let rendered = CliSinkState::format_tool_started("bash_exec", None, |text| {
            state.render_markdown_line(text)
        });

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
        };
        sink.emit(
            AgentEvent::Model(ModelEvent::MessageChunk {
                blocks: vec![ContentBlock::Text("hello".into())],
            }),
            Some(source.clone()),
        );
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
        };
        // Send a chunk to establish source.
        sink.emit(
            AgentEvent::Model(ModelEvent::MessageChunk {
                blocks: vec![ContentBlock::Text("hello".into())],
            }),
            Some(source),
        );
        // End the turn — should reset source tracking.
        sink.emit(
            AgentEvent::Turn(TurnEvent::Ended {
                outcome: Default::default(),
            }),
            None,
        );
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
        };
        // Push two partial chunks without newline.  Buffer should hold both.
        sink.emit(
            AgentEvent::Model(ModelEvent::MessageChunk {
                blocks: vec![ContentBlock::Text("hello ".into())],
            }),
            Some(source.clone()),
        );
        sink.emit(
            AgentEvent::Model(ModelEvent::MessageChunk {
                blocks: vec![ContentBlock::Text("world".into())],
            }),
            Some(source),
        );
        // In non-TTY test process, handle_chunk_text uses handle_raw_chunk
        // (print!), so buffer stays empty for raw path.  We verify the
        // source hasn't reset — i.e. cleanup was NOT called between chunks.
        let state = sink.state.lock().unwrap();
        assert!(
            state.last_ui_output_source.is_some(),
            "source should still be set — cleanup must not run between same-source chunks"
        );
    }
}
