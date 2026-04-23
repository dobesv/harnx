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

use harnx_core::event::{
    AgentEvent, AgentEventSink, ContentBlock, ModelEvent, NoticeEvent, ToolEvent, TurnEvent,
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

struct CliSinkState {
    spinner: Option<Spinner>,
    render: Option<MarkdownRender>,
    buffer: String,
    buffer_rows: u16,
    columns: u16,
    raw_mode_active: bool,
    highlight: bool,
    render_options: RenderOptions,
}

impl CliAgentEventSink {
    pub fn new(highlight: bool, render_options: RenderOptions) -> Self {
        Self {
            state: Arc::new(Mutex::new(CliSinkState {
                spinner: None,
                render: None,
                buffer: String::new(),
                buffer_rows: 1,
                columns: 0,
                raw_mode_active: false,
                highlight,
                render_options,
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
        }
        if self.render.is_none() {
            self.render = Some(MarkdownRender::init(self.render_options.clone())?);
            self.columns = crossterm::terminal::size()?.0;
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
}

impl AgentEventSink for CliAgentEventSink {
    fn emit(&self, event: AgentEvent, _source: Option<harnx_core::event::AgentSource>) {
        // Source-aware CLI rendering (e.g., "> {agent}" prefix for sub-agent
        // output) is a future enhancement. For now, CLI output treats
        // sub-agent events identically to main-agent events, matching the
        // pre-migration baseline where sub-agent chunks on CLI were mostly
        // invisible.
        let _ = _source;
        let mut state = match self.state.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
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
            AgentEvent::Tool(ToolEvent::Started { name, .. }) => {
                eprintln!("{}", dimmed_text(&format!("[tool] {name}")));
            }
            AgentEvent::Tool(ToolEvent::Failed { error, .. }) => {
                eprintln!("{}", warning_text(&format!("tool error: {error}")));
            }
            // Silent for Progress / Update / Completed: CLI doesn't stream
            // per-chunk tool updates; Completed's output is usually internal
            // and shouldn't clutter stderr.
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
        let sink = CliAgentEventSink::new(false, RenderOptions::default());

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
        let sink = CliAgentEventSink::new(false, RenderOptions::default());
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
        let sink = CliAgentEventSink::new(false, RenderOptions::default());
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
}
