//! `CliAgentEventSink` renders `AgentEvent`s for the non-interactive CLI.
//!
//! Streaming chunks (`Model::MessageChunk` / `Model::ThoughtChunk`) are
//! written directly to stdout with optional markdown rendering + raw-mode
//! cursor manipulation — the sink transplants the display logic
//! previously owned by `render::render_stream`. Non-streaming events
//! (notices, errors, usage, tool starts/failures) still go to stderr.

use std::io::{stdout, Stdout, Write};
use std::sync::{Arc, Mutex};

use crossterm::{cursor, queue, style, terminal};
use textwrap::core::display_width;

use harnx_core::abort::AbortSignal;
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
    buffer_rows: u16,
    columns: u16,
    raw_mode_active: bool,
    /// Background task that polls raw-mode key events and wires Ctrl-C/Ctrl-D
    /// to `abort_signal`.  Started when raw mode is enabled; stopped on cleanup.
    key_watcher: Option<harnx_spinner::RawModeKeyWatcher>,
    highlight: bool,
    render_options: RenderOptions,
    /// Propagates Ctrl-C / Ctrl-D key events from raw mode to the caller.
    abort_signal: AbortSignal,
}

impl CliAgentEventSink {
    pub fn new(highlight: bool, render_options: RenderOptions, abort_signal: AbortSignal) -> Self {
        Self {
            state: Arc::new(Mutex::new(CliSinkState {
                spinner: None,
                render: None,
                buffer: String::new(),
                buffer_rows: 1,
                columns: 0,
                raw_mode_active: false,
                key_watcher: None,
                highlight,
                render_options,
                abort_signal,
            })),
        }
    }
}

impl CliSinkState {
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

    /// Markdown streaming path. Logic transplanted verbatim from
    /// `render::stream::markdown_stream_inner`'s SseEvent::Text arm
    /// (harnx commit 8da11d0). References to the outer loop's
    /// `buffer` / `buffer_rows` / `columns` locals become `self.*`
    /// fields on `CliSinkState`.
    fn handle_markdown_chunk(&mut self, text: &str) -> anyhow::Result<()> {
        if !self.raw_mode_active {
            crossterm::terminal::enable_raw_mode()?;
            self.raw_mode_active = true;
            // In raw mode, Ctrl-C does not deliver SIGINT and Ctrl-D does not
            // deliver EOF — both become raw key events.  Start a watcher that
            // reads those key events and forwards them to the abort signal.
            // The watcher is scoped to the raw-mode window and aborted in
            // cleanup() when raw mode is disabled.
            self.key_watcher = harnx_spinner::spawn_raw_mode_key_watcher(self.abort_signal.clone());
        }
        if self.render.is_none() {
            // Any failure here happens after enable_raw_mode() — clean up raw
            // mode (and the key watcher) before propagating the error so the
            // terminal is not left in a corrupted state.
            let init_result = (|| -> anyhow::Result<()> {
                self.render = Some(MarkdownRender::init(self.render_options.clone())?);
                self.columns = crossterm::terminal::size()?.0;
                Ok(())
            })();
            if let Err(e) = init_result {
                let _ = self.cleanup();
                return Err(e);
            }
        }

        let mut writer = stdout();
        // tab width hacking
        let text = text.replace('\t', "    ");

        let mut attempts = 0;
        let (col, mut row) = loop {
            match cursor::position() {
                Ok(pos) => break pos,
                Err(_) if attempts < 3 => attempts += 1,
                Err(e) => return Err(e.into()),
            }
        };

        // Fix unexpected duplicate lines on kitty, see https://github.com/dobesv/harnx/issues/105
        if col == 0 && row > 0 && display_width(&self.buffer) == self.columns as usize {
            row -= 1;
        }

        if row + 1 >= self.buffer_rows {
            queue!(writer, cursor::MoveTo(0, row + 1 - self.buffer_rows),)?;
        } else {
            let scroll_rows = self.buffer_rows - row - 1;
            queue!(
                writer,
                terminal::ScrollUp(scroll_rows),
                cursor::MoveTo(0, 0),
            )?;
        }

        // No guarantee that text returned by render will not be re-layouted, so it is better to clear it.
        queue!(writer, terminal::Clear(terminal::ClearType::FromCursorDown))?;

        let render = self.render.as_mut().expect("render initialized above");

        if text.contains('\n') {
            let text = format!("{}{}", self.buffer, text);
            let (head, tail) = split_line_tail_local(&text);
            let output = render.render(head);
            print_block_local(&mut writer, &output, self.columns)?;
            self.buffer = tail.to_string();
        } else {
            self.buffer = format!("{}{}", self.buffer, text);
        }

        let output = render.render_line(&self.buffer);
        if output.contains('\n') {
            let (head, tail) = split_line_tail_local(&output);
            self.buffer_rows = print_block_local(&mut writer, head, self.columns)?;
            queue!(writer, style::Print(&tail),)?;

            // No guarantee the buffer width of the buffer will not exceed the number of columns.
            // So we calculate the number of rows needed, rather than setting it directly to 1.
            self.buffer_rows += need_rows_local(tail, self.columns);
        } else {
            queue!(writer, style::Print(&output))?;
            self.buffer_rows = need_rows_local(&output, self.columns);
        }

        writer.flush()?;
        Ok(())
    }

    /// End-of-turn / error cleanup: stop spinner, disable raw mode,
    /// emit a trailing newline if the last streamed chunk didn't end
    /// with one, and reset buffers so the next turn starts fresh.
    fn cleanup(&mut self) -> anyhow::Result<()> {
        if let Some(spinner) = self.spinner.take() {
            spinner.stop();
        }
        if self.raw_mode_active {
            // Signal the raw-mode key watcher to stop.  The watcher thread
            // exits within one 25 ms poll slice after seeing the stop flag or
            // a crossterm error from the now-cooked terminal.
            if let Some(watcher) = self.key_watcher.take() {
                watcher.stop();
            }
            crossterm::terminal::disable_raw_mode()?;
            self.raw_mode_active = false;
        }
        // Ensure a trailing newline if we printed something without one.
        if !self.buffer.is_empty() && !self.buffer.ends_with('\n') {
            println!();
        }
        self.buffer.clear();
        self.buffer_rows = 1;
        self.render = None;
        Ok(())
    }

    /// Stderr render for `ToolEvent::Started`: dim "[tool] {name}", and
    /// when the producer rendered an MCP `call_template` into `title`,
    /// append the markdown-styled rendering after it.
    fn print_tool_started(&mut self, name: &str, title: Option<&str>) {
        let prefix = dimmed_text(&format!("[tool] {name}"));
        match title.map(str::trim).filter(|t| !t.is_empty()) {
            Some(t) => {
                let rendered = self.render_markdown_line(t);
                eprintln!("{prefix} {rendered}");
            }
            None => eprintln!("{prefix}"),
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
    fn print_tool_completed(&mut self, output: &serde_json::Value, title: Option<&str>) {
        let text = harnx_runtime::utils::render_tool_result_text(output, title);
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
        let show_heading = matches!(
            event,
            AgentEvent::Model(ModelEvent::MessageChunk { .. })
                | AgentEvent::Model(ModelEvent::ThoughtChunk { .. })
                | AgentEvent::Model(ModelEvent::Final { .. })
                | AgentEvent::Model(ModelEvent::Error(_))
        );
        if show_heading {
            if let Some(source) = source.as_ref() {
                if let Err(err) = state.cleanup() {
                    eprintln!(
                        "{}",
                        warning_text(&format!("cli-sink cleanup failed: {err}"))
                    );
                }
                println!("{}", source_heading(source));
            }
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
            AgentEvent::Tool(ToolEvent::Started { name, title, .. }) => {
                state.print_tool_started(&name, title.as_deref());
            }
            AgentEvent::Tool(ToolEvent::Failed { error, .. }) => {
                eprintln!("{}", warning_text(&format!("tool error: {error}")));
            }
            AgentEvent::Tool(ToolEvent::Completed { output, title, .. }) => {
                state.print_tool_completed(&output, title.as_deref());
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
// Module-private helpers — mirror the free functions in render/stream.rs so
// that the markdown rendering body above stays byte-for-byte equivalent to
// the pre-plan implementation.

fn print_block_local(writer: &mut Stdout, text: &str, columns: u16) -> anyhow::Result<u16> {
    let mut num = 0;
    for line in text.split('\n') {
        queue!(
            writer,
            style::Print(line),
            style::Print("\n"),
            cursor::MoveLeft(columns),
        )?;
        num += 1;
    }
    Ok(num)
}

fn split_line_tail_local(text: &str) -> (&str, &str) {
    if let Some((head, tail)) = text.rsplit_once('\n') {
        (head, tail)
    } else {
        ("", text)
    }
}

fn need_rows_local(text: &str, columns: u16) -> u16 {
    if columns == 0 {
        return 0;
    }
    let buffer_width = display_width(text).max(1) as u16;
    buffer_width.div_ceil(columns)
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::event::{ContentBlock, StatusLine};

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
    // MCP MiniJinja templating: CLI must surface the rendered title/
    // content fields produced by harnx-runtime when an MCP tool's
    // `_meta.call_template` / `_meta.result_template` is set. Covers
    // issue #340 / PR #349 — the producer wired templates into the
    // ToolEvent fields, but the CLI consumer was discarding them
    // prior to these tests.
    // ----------------------------------------------------------------

    // The CLI Started handler renders the bare "[tool] name" prefix
    // dimmed and appends the markdown-rendered title (or nothing if no
    // title). The whitespace/empty fallback decision lives inline in the
    // emit handler, exercised end-to-end by
    // `emit_handles_each_top_level_variant_without_panic` plus the
    // markdown-line tests below.

    // The CLI Completed handler delegates to
    // `harnx_runtime::utils::render_tool_result_text`, the same shared
    // helper the TUI uses. We assert against that helper directly so any
    // future tweak to the rendering rules updates a single test surface.

    #[test]
    fn completed_uses_template_title_when_present() {
        // With a template-rendered title, prefer it over the raw output.
        let raw_output = serde_json::json!({
            "content": [{"type": "text", "text": "hello"}],
            "isError": false,
        });
        let rendered =
            harnx_runtime::utils::render_tool_result_text(&raw_output, Some("OK: hello"));
        assert!(rendered.contains("OK: hello"));
        assert!(
            !rendered.contains("isError"),
            "raw output JSON leaked when title was provided: {rendered}"
        );
    }

    #[test]
    fn completed_falls_back_to_extracted_text_when_no_title() {
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
    fn completed_falls_back_to_string_when_no_title_and_string_output() {
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
    fn completed_treats_empty_title_as_no_title() {
        // A template that renders to "" must not blank out the result —
        // fall back to extraction so the user still sees the tool's work.
        let raw_output = serde_json::Value::String("important output".into());
        let rendered = harnx_runtime::utils::render_tool_result_text(&raw_output, Some(""));
        assert!(
            rendered.contains("important output"),
            "empty title should fall back to extraction: {rendered}"
        );
    }

    // ----------------------------------------------------------------
    // Markdown rendering for tool events. The state.render_markdown_line
    // helper drops back to plain text when highlighting is disabled or
    // the renderer can't initialize, otherwise it produces ANSI-styled
    // output via syntect.
    // ----------------------------------------------------------------

    fn make_state(highlight: bool) -> CliSinkState {
        CliSinkState {
            spinner: None,
            render: None,
            buffer: String::new(),
            buffer_rows: 1,
            columns: 0,
            raw_mode_active: false,
            key_watcher: None,
            highlight,
            render_options: RenderOptions::default(),
            abort_signal: harnx_core::abort::create_abort_signal(),
        }
    }

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
}
