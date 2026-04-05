use super::{MarkdownRender, SseEvent};

use crate::repl::input_queue::InputQueue;
use crate::utils::{poll_abort_signal, poll_abort_signal_with_input, spawn_spinner, AbortSignal};

use anyhow::Result;
use crossterm::{
    cursor, queue, style,
    terminal::{self, disable_raw_mode, enable_raw_mode},
};
use std::{
    io::{self, stdout, Stdout, Write},
    time::Duration,
};
use textwrap::core::display_width;
use tokio::sync::mpsc::UnboundedReceiver;

pub async fn markdown_stream(
    rx: UnboundedReceiver<SseEvent>,
    render: &mut MarkdownRender,
    abort_signal: &AbortSignal,
    spinner_message: &str,
    input_queue: Option<&InputQueue>,
) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();

    let ret = markdown_stream_inner(
        rx,
        render,
        abort_signal,
        &mut stdout,
        spinner_message,
        input_queue,
    )
    .await;

    disable_raw_mode()?;

    if ret.is_err() {
        println!();
    }
    ret
}

pub async fn raw_stream(
    mut rx: UnboundedReceiver<SseEvent>,
    abort_signal: &AbortSignal,
    spinner_message: &str,
    _input_queue: Option<&InputQueue>,
) -> Result<()> {
    let mut spinner = Some(spawn_spinner(spinner_message));

    loop {
        if abort_signal.aborted() {
            break;
        }
        if let Some(evt) = rx.recv().await {
            if let Some(spinner) = spinner.take() {
                spinner.stop();
            }

            match evt {
                SseEvent::Text(text) => {
                    print!("{text}");
                    stdout().flush()?;
                }
                SseEvent::Done => {
                    break;
                }
            }
        }
    }
    if let Some(spinner) = spinner.take() {
        spinner.stop();
    }
    Ok(())
}

async fn markdown_stream_inner(
    mut rx: UnboundedReceiver<SseEvent>,
    render: &mut MarkdownRender,
    abort_signal: &AbortSignal,
    writer: &mut Stdout,
    spinner_message: &str,
    input_queue: Option<&InputQueue>,
) -> Result<()> {
    let mut buffer = String::new();
    let mut buffer_rows = 1u16;

    let (columns, term_height) = terminal::size()?;

    // Calculate footer rows needed (spinner + input)
    let needs_spinner = !spinner_message.is_empty();
    let has_input = input_queue.is_some();
    let footer_rows = match (needs_spinner, has_input) {
        (true, true) => 2,
        (true, false) => 1,
        (false, true) => 1,
        (false, false) => 0,
    };

    // Calculate the content area height (leave last footer_rows rows for footer)
    let content_height = term_height.saturating_sub(footer_rows);

    // Local spinner state - we control it synchronously for coordinated rendering
    let mut spinner_index: usize = 0;
    let mut spinner_stopped = false;

    'outer: loop {
        if abort_signal.aborted() {
            break;
        }

        for reply_event in gather_events(&mut rx).await {
            if !spinner_stopped {
                // Stop the external spinner on first content - we'll manage our own
                // Note: we don't have access to it here, it was started elsewhere.
                // Instead, we'll just set a flag and render our own spinner in footer.
                spinner_stopped = true;
            }

            match reply_event {
                SseEvent::Text(mut text) => {
                    // tab width hacking
                    text = text.replace('\t', "    ");

                    // Get current cursor position
                    let mut attempts = 0;
                    let (col, mut row) = loop {
                        match cursor::position() {
                            Ok(pos) => break pos,
                            Err(_) if attempts < 3 => attempts += 1,
                            Err(e) => return Err(e.into()),
                        }
                    };

                    // Fix unexpected duplicate lines on kitty, see https://github.com/dobesv/harnx/issues/105
                    if col == 0 && row > 0 && display_width(&buffer) == columns as usize {
                        row -= 1;
                    }

                    // Content cursor should be constrained to content area (above footer)
                    let content_cursor_row = row.min(content_height.saturating_sub(1));

                    // Move to the correct content position
                    queue!(writer, cursor::MoveTo(0, content_cursor_row))?;

                    // Scroll if needed to fit content within content area
                    if buffer_rows > content_height && content_height > 0 {
                        let scroll_rows = buffer_rows - content_height;
                        queue!(writer, terminal::ScrollUp(scroll_rows))?;
                        queue!(writer, cursor::MoveTo(0, content_height.saturating_sub(1)))?;
                    }

                    // Clear from cursor down (this clears content area, footer is rendered separately)
                    queue!(writer, terminal::Clear(terminal::ClearType::FromCursorDown))?;

                    if text.contains('\n') {
                        let text = format!("{buffer}{text}");
                        let (head, tail) = split_line_tail(&text);
                        let output = render.render(head);
                        print_block(writer, &output, columns)?;
                        buffer = tail.to_string();
                    } else {
                        buffer = format!("{buffer}{text}");
                    }

                    let output = render.render_line(&buffer);
                    if output.contains('\n') {
                        let (head, tail) = split_line_tail(&output);
                        buffer_rows = print_block(writer, head, columns)?;
                        queue!(writer, style::Print(&tail),)?;

                        // No guarantee the buffer width of the buffer will not exceed the number of columns.
                        // So we calculate the number of rows needed, rather than setting it directly to 1.
                        buffer_rows += need_rows(tail, columns);
                    } else {
                        queue!(writer, style::Print(&output))?;
                        buffer_rows = need_rows(&output, columns);
                    }

                    // Render footer at absolute bottom position
                    if footer_rows > 0 {
                        render_footer_absolute(
                            writer,
                            render,
                            columns,
                            term_height,
                            needs_spinner,
                            spinner_message,
                            &mut spinner_index,
                            input_queue,
                        )?;
                    }

                    writer.flush()?;
                }
                SseEvent::Done => {
                    break 'outer;
                }
            }
        }

        let aborted = match input_queue {
            Some(iq) => poll_abort_signal_with_input(abort_signal, iq)?,
            None => poll_abort_signal(abort_signal)?,
        };

        // Update footer on each loop iteration (spinner animation + input changes)
        if footer_rows > 0 {
            render_footer_absolute(
                writer,
                render,
                columns,
                term_height,
                needs_spinner,
                spinner_message,
                &mut spinner_index,
                input_queue,
            )?;
            writer.flush()?;
        }

        if aborted {
            break;
        }
    }

    // Clear footer on exit
    if footer_rows > 0 {
        // Move to start of footer area and clear down
        let footer_start_row = term_height.saturating_sub(footer_rows);
        queue!(
            writer,
            cursor::MoveTo(0, footer_start_row),
            terminal::Clear(terminal::ClearType::FromCursorDown),
            cursor::Show
        )?;
        writer.flush()?;
    }

    Ok(())
}

/// Render the footer using absolute cursor positioning.
/// The footer is pinned to the last rows of the terminal.
#[allow(clippy::too_many_arguments)]
fn render_footer_absolute(
    writer: &mut Stdout,
    render: &mut MarkdownRender,
    _columns: u16,
    term_height: u16,
    needs_spinner: bool,
    spinner_message: &str,
    spinner_index: &mut usize,
    input_queue: Option<&InputQueue>,
) -> Result<()> {
    let has_input = input_queue.is_some();
    let footer_rows = match (needs_spinner, has_input) {
        (true, true) => 2,
        (true, false) => 1,
        (false, true) => 1,
        (false, false) => 0,
    };

    if footer_rows == 0 {
        return Ok(());
    }

    // Calculate footer start row (0-indexed from top of terminal)
    let footer_start_row = term_height.saturating_sub(footer_rows);

    // Hide cursor during footer rendering
    queue!(writer, cursor::Hide)?;

    // Clear each footer line and then write content
    for i in 0..footer_rows {
        let row = footer_start_row + i;
        queue!(
            writer,
            cursor::MoveTo(0, row),
            terminal::Clear(terminal::ClearType::CurrentLine)
        )?;
    }

    // Now render the footer content at absolute positions
    let mut current_row = footer_start_row;

    // Render spinner line if needed
    if needs_spinner {
        queue!(writer, cursor::MoveTo(0, current_row))?;
        let frame = SPINNER_FRAMES[*spinner_index % SPINNER_FRAMES.len()];
        let spinner_line = format!("{frame}{spinner_message}");
        queue!(writer, style::Print(&spinner_line))?;
        *spinner_index = spinner_index.wrapping_add(1);
        current_row += 1;
    }

    // Always render the input prompt line when an input queue is available.
    if let Some(iq) = input_queue {
        queue!(writer, cursor::MoveTo(0, current_row))?;
        let display = iq.get_display_line();
        let input_output = render.render_line(&display);
        queue!(writer, style::Print(&input_output))?;
    }

    // Show cursor again for input
    queue!(writer, cursor::Show)?;

    Ok(())
}

/// Spinner frame characters (same as SpinnerInner::DATA)
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

async fn gather_events(rx: &mut UnboundedReceiver<SseEvent>) -> Vec<SseEvent> {
    let mut texts = vec![];
    let mut done = false;
    tokio::select! {
        _ = async {
            while let Some(reply_event) = rx.recv().await {
                match reply_event {
                    SseEvent::Text(v) => texts.push(v),
                    SseEvent::Done => {
                        done = true;
                        break;
                    }
                }
            }
        } => {}
        _ = tokio::time::sleep(Duration::from_millis(50)) => {}
    };
    let mut events = vec![];
    if !texts.is_empty() {
        events.push(SseEvent::Text(texts.join("")))
    }
    if done {
        events.push(SseEvent::Done)
    }
    events
}

fn print_block(writer: &mut Stdout, text: &str, columns: u16) -> Result<u16> {
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

fn split_line_tail(text: &str) -> (&str, &str) {
    if let Some((head, tail)) = text.rsplit_once('\n') {
        (head, tail)
    } else {
        ("", text)
    }
}

fn need_rows(text: &str, columns: u16) -> u16 {
    let buffer_width = display_width(text).max(1) as u16;
    buffer_width.div_ceil(columns)
}
