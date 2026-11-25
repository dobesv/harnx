use crate::hyperlink::HyperlinkWriter;
use crate::types::Tui;
use anyhow::Result;
use crossterm::cursor::{MoveTo, Show};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::queue;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement, Clear, ClearType,
    EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders};
use ratatui::Terminal;
use ratatui_textarea::TextArea;
use std::io::{self, IsTerminal, Stdout, Write};
use std::panic;
use std::sync::Mutex;

/// Raw CUP (`ESC[9999;1H`) for the bottom-row fallback when the terminal size
/// is unknown: row 9999 exceeds any screen, and terminals clamp an
/// out-of-range row to the last row; column 1 leaves no pending-wrap state.
///
/// Raw bytes instead of crossterm's `MoveTo(9999, 1)` on purpose: `MoveTo`
/// takes `(col, row)`, so that call would move to row 1. On a non-VT Windows
/// console — where `terminal::size()` has already failed to reach this arm —
/// the bytes may be ignored or printed; acceptable for a best-effort
/// fallback.
const MOVE_TO_BOTTOM_ROW: &[u8] = b"\x1b[9999;1H";

/// Cursor position on the normal screen. Named fields (rather than
/// crossterm's `(col, row)` tuple) keep the coordinates from being swapped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CursorPosition {
    row: u16,
    col: u16,
}

/// Terminal size in cells, matching `crossterm::terminal::size()` order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TerminalSize {
    width: u16,
    height: u16,
}

/// Cursor position plus terminal size, captured together just before the
/// alternate screen is entered (#2913). Teardown replays the position after
/// `LeaveAlternateScreen` because its DECRC cursor restore can land on (0,0)
/// when the terminal loses the saved cursor (multiplexers, resize, editor
/// suspend/resume), and then the post-exit stderr messages overwrite the
/// restored pre-TUI screen content. The size is what makes the position
/// trustworthy at teardown: a resize or reflow moves content relative to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SavedCursor {
    position: CursorPosition,
    /// `None` when the size could not be read at capture time; teardown then
    /// cannot validate the position and falls back.
    size: Option<TerminalSize>,
}

/// Saved pre-alternate-screen cursor state. Production uses the process-global
/// [PRE_ALTERNATE_CURSOR]; tests construct their own with injected TTY gate,
/// position probe and size probe.
#[derive(Default)]
struct SavedCursorState {
    saved: Mutex<Option<SavedCursor>>,
}

static PRE_ALTERNATE_CURSOR: SavedCursorState = SavedCursorState {
    saved: Mutex::new(None),
};

impl SavedCursorState {
    /// Capture the cursor position and terminal size from the normal screen.
    ///
    /// Every capture overwrites the previous one, including the failed ones
    /// (`is_tty` false or the probe returning `None`): a stale row from an
    /// earlier capture could otherwise erase restored content at teardown
    /// (#2913). `probe` returns crossterm's `(col, row)` order and is only
    /// called on a TTY.
    ///
    /// The poisoned-lock recovery repeats in `take` on purpose: a shared
    /// `lock()` helper there trips rustqual's cross-file `lock()`/`take()`
    /// idiom cluster (see the quality gate), and the two sites are clearer
    /// spelled out than routed through indirection.
    fn capture(
        &self,
        is_tty: impl FnOnce() -> bool,
        probe: impl FnOnce() -> Option<(u16, u16)>,
        mut size: impl FnMut() -> Option<TerminalSize>,
    ) {
        let saved = if is_tty() {
            let size_before = size();
            let position = probe();
            let size_after = size();
            match (size_before, position, size_after) {
                (Some(before), Some((col, row)), Some(after)) if before == after => {
                    Some(SavedCursor {
                        position: CursorPosition { row, col },
                        size: Some(before),
                    })
                }
                _ => None,
            }
        } else {
            None
        };
        // Poison-safety: recover a poisoned lock — the guarded value is a
        // plain Option, never an invariant, and capture must not break.
        *self
            .saved
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = saved;
    }

    /// Take the saved cursor so teardown places it exactly once: a second
    /// cleanup (the panic hook after the editor suspend hook, say) must not
    /// replay a stale position and its clear (#2913).
    fn take(&self) -> Option<SavedCursor> {
        // Poison-safety: as in `capture`.
        self.saved
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    /// Drop the saved cursor without capturing a new one. Used after a
    /// resize: the saved row was recorded against a screen that no longer
    /// exists, and clearing from it could erase reflowed content (#2913).
    fn clear(&self) {
        // Poison-safety: as in `capture`.
        *self
            .saved
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }
}

/// Capture the pre-TUI cursor position into [PRE_ALTERNATE_CURSOR] (#2913).
///
/// Call with raw mode already enabled, right before `EnterAlternateScreen`.
/// Best-effort: skipped when stdout is not a TTY. On a TTY that never answers
/// the cursor-position request, crossterm waits out its internal 2 s poll
/// (once its event reader initializes) and returns an error, so a failed
/// probe only falls back to the bottom-of-screen heuristic at teardown.
fn capture_pre_alternate_cursor() {
    PRE_ALTERNATE_CURSOR.capture(
        || io::stdout().is_terminal(),
        || crossterm::cursor::position().ok(),
        current_terminal_size,
    );
}

fn current_terminal_size() -> Option<TerminalSize> {
    crossterm::terminal::size()
        .ok()
        .map(|(width, height)| TerminalSize { width, height })
}

/// Drop the saved pre-TUI cursor position after a resize. The saved row is
/// only trustworthy while the screen kept its exact size: content reflows
/// under it, and `Clear(FromCursorDown)` at teardown could then erase the
/// tail of the restored content (#2913). Teardown falls back to the
/// bottom-row+newline placement instead.
pub(super) fn invalidate_pre_alternate_cursor() {
    PRE_ALTERNATE_CURSOR.clear();
}

/// Where teardown puts the cursor after `LeaveAlternateScreen`.
#[derive(Debug, PartialEq, Eq)]
enum ExitCursorPlacement {
    /// Back where the pre-TUI content ended (column clamped to the current
    /// width), with the screen cleared from there down, so post-exit output
    /// lands on empty lines instead of over the restored screen content.
    SavedPosition { row: u16, col: u16 },
    /// On a fresh line below all prior content: move to the bottom row and
    /// emit a newline, which scrolls the screen one line. Used when the saved
    /// position is unknown, sits at row 0 (prior content can reach the bottom
    /// of the screen, so nothing below it is safe to overwrite) or fell
    /// victim to a resize or reflow. `bottom_row` is `None` when the current
    /// terminal size is unknown or unusable; the emitted row is then
    /// out-of-range on purpose, so the terminal clamps it to the last row.
    BottomNewline { bottom_row: Option<u16> },
}

fn compute_exit_cursor_placement(
    saved: Option<SavedCursor>,
    current_size: Option<TerminalSize>,
) -> ExitCursorPlacement {
    let Some(size) = current_size.filter(|size| size.width > 0 && size.height > 0) else {
        // An unusable size cannot validate the saved position. A bare newline
        // would overwrite mid-screen when the terminal's own DECRC restore
        // also failed (#2913), so use the clamped move-to-bottom instead.
        return ExitCursorPlacement::BottomNewline { bottom_row: None };
    };
    match saved {
        // The saved row is only trustworthy while the screen kept its exact
        // size: a resize or reflow moves content relative to it, and
        // `Clear(FromCursorDown)` would then erase restored content.
        Some(saved)
            if saved.size == Some(size)
                && (1..size.height).contains(&saved.position.row)
                && saved.position.col < size.width - 1 =>
        {
            ExitCursorPlacement::SavedPosition {
                row: saved.position.row,
                col: saved.position.col.min(size.width - 1),
            }
        }
        _ => ExitCursorPlacement::BottomNewline {
            bottom_row: Some(size.height - 1),
        },
    }
}

fn write_exit_cursor_sequences<W: Write>(
    writer: &mut W,
    placement: ExitCursorPlacement,
) -> io::Result<()> {
    match placement {
        ExitCursorPlacement::SavedPosition { row, col } => {
            queue!(writer, MoveTo(col, row), Clear(ClearType::FromCursorDown))?;
        }
        ExitCursorPlacement::BottomNewline { bottom_row } => {
            match bottom_row {
                Some(bottom_row) => queue!(writer, MoveTo(0, bottom_row))?,
                None => writer.write_all(MOVE_TO_BOTTOM_ROW)?,
            }
            // Raw mode is already off here, so the newline scrolls the screen
            // and leaves the cursor on a fresh line below all prior content.
            writer.write_all(b"\n")?;
        }
    }
    Ok(())
}

/// Explicitly place the cursor for the output that follows teardown; the
/// terminal's own `?1049` cursor restore cannot be trusted (#2913). Consumes
/// the saved position: teardown runs at most once per capture.
fn place_saved_cursor_after_leave<W: Write>(
    state: &SavedCursorState,
    writer: &mut W,
    current_size: Option<TerminalSize>,
) -> io::Result<()> {
    // The size probe is evaluated before take() runs, and take() releases
    // the lock before returning — no lock is held across the probe or the
    // writes below.
    let placement = compute_exit_cursor_placement(state.take(), current_size);
    write_exit_cursor_sequences(writer, placement)
}

fn place_cursor_after_leave<W: Write>(writer: &mut W) -> io::Result<()> {
    place_saved_cursor_after_leave(&PRE_ALTERNATE_CURSOR, writer, current_terminal_size())
}

/// Tear down terminal modes and place the cursor for the output that follows.
///
/// Shared by the normal exit (`restore_terminal`), the panic hook and the
/// external-editor suspend hook, so every path ends with the cursor visible
/// at the end of the restored content, or below all of it (#2913).
pub(super) fn cleanup_terminal_state() {
    let _ = disable_raw_mode();
    let mut stdout = io::stdout();
    // Skip terminal escape-sequence cleanup when stdout is not an interactive
    // TTY (e.g. under `cargo nextest`, piped output, or headless CI). The
    // `supports_keyboard_enhancement()` query below writes a terminal request
    // and blocks waiting for a reply that never arrives in those environments,
    // hanging the process. `disable_raw_mode()` above is always safe to run.
    if !stdout.is_terminal() {
        return;
    }
    // Clear terminal status indicator before leaving alternate screen.
    // Safe during panic: force_clear uses lock().ok() and ignores write errors.
    crate::terminal_status::force_clear();
    if supports_keyboard_enhancement().unwrap_or(false) {
        let _ = stdout.execute(PopKeyboardEnhancementFlags);
    }
    let _ = stdout.execute(DisableBracketedPaste);
    let _ = stdout.execute(DisableMouseCapture);
    let _ = stdout.execute(crossterm::terminal::SetTitle("harnx"));
    let _ = stdout.execute(LeaveAlternateScreen);
    // `LeaveAlternateScreen` relies on DECRC to restore the cursor, which
    // sometimes lands on (0,0); place it at the end of the restored content
    // explicitly instead (#2913).
    let _ = place_cursor_after_leave(&mut stdout);
    // Show the cursor here, not only in `restore_terminal`, so the panic hook
    // path — which runs this same cleanup — leaves a visible cursor.
    let _ = stdout.execute(Show);
    let _ = stdout.flush();
}

type PanicHook = dyn Fn(&panic::PanicHookInfo<'_>) + Sync + Send + 'static;

pub(super) struct PanicTerminalHookGuard {
    previous_hook: Option<std::sync::Arc<PanicHook>>,
}

impl PanicTerminalHookGuard {
    pub(super) fn install() -> Self {
        let previous_hook: std::sync::Arc<PanicHook> = std::sync::Arc::from(panic::take_hook());
        let chained_hook = previous_hook.clone();
        panic::set_hook(Box::new(move |panic_info: &panic::PanicHookInfo<'_>| {
            cleanup_terminal_state();
            chained_hook(panic_info);
        }));
        Self {
            previous_hook: Some(previous_hook),
        }
    }
}

impl Drop for PanicTerminalHookGuard {
    fn drop(&mut self) {
        // `panic::set_hook` itself panics ("cannot modify the panic hook from a
        // panicking thread") when called while a panic is unwinding. If this
        // guard is dropped as part of that unwinding, restoring the previous
        // hook would turn a normal panic into a fatal double-panic ("panic in a
        // destructor during cleanup") that aborts the process. Skip restoration
        // in that case — the installed hook has already restored the terminal,
        // and the panic can continue to unwind normally.
        if std::thread::panicking() {
            return;
        }
        if let Some(previous_hook) = self.previous_hook.take() {
            panic::set_hook(Box::new(move |panic_info: &panic::PanicHookInfo<'_>| {
                previous_hook(panic_info);
            }));
        }
    }
}

impl Tui {
    pub(super) fn new_input() -> TextArea<'static> {
        let mut input = TextArea::default();
        input.set_block(
            Block::default()
                .borders(Borders::TOP)
                .title("Input")
                .border_style(Style::default()),
        );
        input.set_cursor_line_style(Style::default());
        input.set_style(Style::default().fg(Color::White));
        input.set_wrap_mode(ratatui_textarea::WrapMode::Word);
        // input.set_placeholder_text("Enter submits · Shift+Enter / Ctrl+J for newline");
        input
    }

    pub(super) fn setup_terminal() -> Result<Terminal<CrosstermBackend<HyperlinkWriter<Stdout>>>> {
        let stdout = enter_terminal_modes()?;
        Ok(Terminal::new(CrosstermBackend::new(HyperlinkWriter::new(
            stdout,
            hyperlink_emission_ok(),
        )))?)
    }

    pub(super) fn restore_terminal(
        terminal: &mut Terminal<CrosstermBackend<HyperlinkWriter<Stdout>>>,
    ) -> Result<()> {
        cleanup_terminal_state();
        terminal.show_cursor()?;
        Ok(())
    }
}

/// Enter raw mode, probe kitty keyboard-enhancement support, capture the
/// pre-TUI cursor, then switch to the alternate screen with mouse capture
/// and bracketed paste, returning the configured stdout.
fn enter_terminal_modes() -> Result<Stdout> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // Querying keyboard enhancement before entering the alternate screen also
    // means its failure surfaces before the switch, instead of stranding the
    // terminal inside the alternate screen (#1879's setup path).
    enter_alternate_screen_modes(
        &mut stdout,
        // The probe writes its own DSR query to stdout inside crossterm.
        |_| capture_pre_alternate_cursor(),
        supports_keyboard_enhancement()?,
    )?;
    stdout.flush()?;
    Ok(stdout)
}

/// Re-enter terminal modes after the external editor exits (raw mode must be
/// re-enabled by the caller first). Re-captures the cursor first, so the final
/// teardown lands below the editor's output rather than at the stale pre-TUI
/// position (#2913). Best-effort, like the hook that calls it.
pub(super) fn reenter_terminal_modes(stdout: &mut Stdout) {
    let _ = enter_alternate_screen_modes(
        stdout,
        |_| capture_pre_alternate_cursor(),
        supports_keyboard_enhancement().unwrap_or(false),
    );
    let _ = stdout.flush();
}

/// Switch to the alternate screen with mouse capture and bracketed paste.
///
/// `capture` runs before `EnterAlternateScreen` hides the normal screen: it
/// records the pre-TUI cursor position that teardown restores (#2913).
/// `keyboard_enhancement` decides whether the kitty keyboard flags are
/// pushed; a terminal that did not report support must not see the sequence.
fn enter_alternate_screen_modes<W: Write>(
    writer: &mut W,
    capture: impl FnOnce(&mut W),
    keyboard_enhancement: bool,
) -> Result<()> {
    capture(writer);
    queue!(writer, EnterAlternateScreen)?;
    if keyboard_enhancement {
        queue!(
            writer,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_EVENT_TYPES,
            )
        )?;
    }
    queue!(writer, EnableMouseCapture)?;
    queue!(writer, EnableBracketedPaste)?;
    Ok(())
}

/// Whether the terminal can be assumed to render OSC 8 hyperlinks: same
/// static gate as the terminal status indicator (TTY, TERM != dumb, CI unset).
/// When false the hyperlink writer still strips its sentinel characters, so
/// the URL text renders as plain text.
fn hyperlink_emission_ok() -> bool {
    use std::io::IsTerminal;
    io::stdout().is_terminal()
        && std::env::var("TERM").as_deref() != Ok("dumb")
        && std::env::var("CI").is_err()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(windows))]
    const MOUSE_CAPTURE: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1015h\x1b[?1006h";

    fn position(row: u16, col: u16) -> CursorPosition {
        CursorPosition { row, col }
    }

    fn size(width: u16, height: u16) -> TerminalSize {
        TerminalSize { width, height }
    }

    fn saved(row: u16, col: u16, size: TerminalSize) -> SavedCursor {
        SavedCursor {
            position: position(row, col),
            size: Some(size),
        }
    }

    /// Read or write the saved state directly, bypassing the capture seams.
    /// Poison-tolerant so tests can inspect a mutex poisoned on purpose.
    fn set_saved(state: &SavedCursorState, value: Option<SavedCursor>) {
        *state
            .saved
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = value;
    }

    fn saved_now(state: &SavedCursorState) -> Option<SavedCursor> {
        *state
            .saved
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    // ---- capture glue: SavedCursorState with injected TTY/probe/size ----

    #[test]
    fn capture_skips_probe_and_resets_state_when_not_a_tty() {
        let state = SavedCursorState::default();
        set_saved(&state, Some(saved(5, 10, size(80, 24))));
        state.capture(
            || false,
            // The probes must never run on a non-TTY: the DSR query would
            // block waiting for a reply that never arrives.
            || -> Option<(u16, u16)> { panic!("probe must not run on a non-TTY") },
            || -> Option<TerminalSize> { panic!("size probe must not run on a non-TTY") },
        );
        assert_eq!(
            saved_now(&state),
            None,
            "a skipped capture must clear stale state"
        );
    }

    #[test]
    fn capture_maps_crossterm_col_row_pair_without_swapping() {
        let state = SavedCursorState::default();
        // crossterm reports (col, row); an asymmetrical pair catches a swap.
        state.capture(|| true, || Some((3, 7)), || Some(size(80, 24)));
        assert_eq!(saved_now(&state), Some(saved(7, 3, size(80, 24))));
    }

    #[test]
    fn capture_failed_probe_resets_previous_state() {
        let state = SavedCursorState::default();
        set_saved(&state, Some(saved(5, 10, size(80, 24))));
        state.capture(|| true, || None, || Some(size(80, 24)));
        assert_eq!(
            saved_now(&state),
            None,
            "a failed probe must invalidate the previously saved position"
        );
    }

    #[test]
    fn capture_discards_position_when_size_probe_fails() {
        let state = SavedCursorState::default();
        state.capture(|| true, || Some((0, 5)), || None);
        assert_eq!(saved_now(&state), None);
    }

    #[test]
    fn capture_requires_stable_size_around_position_probe() {
        let changed = SavedCursorState::default();
        set_saved(&changed, Some(saved(5, 10, size(80, 24))));
        let mut sizes = [Some(size(80, 24)), Some(size(60, 24))].into_iter();
        changed.capture(|| true, || Some((3, 7)), || sizes.next().flatten());
        assert_eq!(saved_now(&changed), None);

        let stable = SavedCursorState::default();
        let mut sizes = [Some(size(80, 24)), Some(size(80, 24))].into_iter();
        stable.capture(|| true, || Some((3, 7)), || sizes.next().flatten());
        assert_eq!(saved_now(&stable), Some(saved(7, 3, size(80, 24))));
    }

    #[test]
    fn capture_recovers_from_poisoned_state_lock() {
        let state = SavedCursorState::default();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = state.saved.lock().unwrap();
            panic!("poison the lock");
        }));
        assert!(result.is_err());
        state.capture(|| true, || Some((1, 2)), || Some(size(80, 24)));
        assert_eq!(saved_now(&state), Some(saved(2, 1, size(80, 24))));
    }

    #[test]
    fn take_recovers_from_poisoned_state_lock() {
        let state = SavedCursorState::default();
        set_saved(&state, Some(saved(5, 10, size(80, 24))));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = state.saved.lock().unwrap();
            panic!("poison the lock");
        }));
        assert!(result.is_err());
        // A poisoned lock must not break teardown: take() still consumes the
        // saved position exactly once.
        assert_eq!(state.take(), Some(saved(5, 10, size(80, 24))));
        assert_eq!(saved_now(&state), None);
    }

    #[test]
    fn clear_drops_saved_cursor_so_teardown_falls_back() {
        let state = SavedCursorState::default();
        set_saved(&state, Some(saved(5, 10, size(80, 24))));
        state.clear();
        assert_eq!(saved_now(&state), None);
        // The placement teardown then computes is the unknown-saved-position
        // fallback, covered by exit_placement_falls_back_when_saved_position_unknown.
    }

    // The byte assertions below are Unix-only. On Windows, crossterm's
    // `queue!` routes a command through WinAPI instead of the writer —
    // writing nothing to the Vec — whenever `is_ansi_code_supported()` is
    // false: always for mouse capture and the kitty keyboard flags, and for
    // cursor/terminal commands when the console is not VT-enabled (an
    // in-memory Vec<u8> never is). The decision logic stays covered
    // portably by the compute_exit_cursor_placement tests above.
    // ---- placement glue: state take + writer output ----

    #[cfg(not(windows))]
    #[test]
    fn placement_restores_saved_cursor_and_consumes_state() {
        let state = SavedCursorState::default();
        set_saved(&state, Some(saved(5, 10, size(80, 24))));
        let mut out = Vec::new();
        place_saved_cursor_after_leave(&state, &mut out, Some(size(80, 24))).unwrap();
        assert_eq!(out, b"\x1b[6;11H\x1b[J".as_slice());
        assert_eq!(
            saved_now(&state),
            None,
            "placement must consume the saved state"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn second_placement_after_consumed_state_uses_bottom_newline() {
        let state = SavedCursorState::default();
        set_saved(&state, Some(saved(5, 10, size(80, 24))));
        let mut first = Vec::new();
        place_saved_cursor_after_leave(&state, &mut first, Some(size(80, 24))).unwrap();
        let mut second = Vec::new();
        place_saved_cursor_after_leave(&state, &mut second, Some(size(80, 24))).unwrap();
        // A second cleanup (the panic hook after the editor suspend hook) must
        // not replay the stale position; scroll below the content instead.
        assert_eq!(second, b"\x1b[24;1H\n".as_slice());
    }

    #[cfg(not(windows))]
    #[test]
    fn placement_falls_back_when_terminal_size_changed_since_capture() {
        // A narrower terminal rewraps the pre-TUI content, so the saved row no
        // longer points at its end; clearing there would erase it.
        let state = SavedCursorState::default();
        set_saved(&state, Some(saved(5, 10, size(80, 24))));
        let mut out = Vec::new();
        place_saved_cursor_after_leave(&state, &mut out, Some(size(60, 24))).unwrap();
        assert_eq!(out, b"\x1b[24;1H\n".as_slice());
    }

    // ---- exit placement decision ----

    #[test]
    fn exit_placement_reuses_saved_position_within_bounds() {
        let placement =
            compute_exit_cursor_placement(Some(saved(5, 10, size(80, 24))), Some(size(80, 24)));
        assert_eq!(
            placement,
            ExitCursorPlacement::SavedPosition { row: 5, col: 10 }
        );
    }

    #[test]
    fn exit_placement_accepts_row_boundaries_one_and_last_row() {
        for row in [1, 23] {
            let placement = compute_exit_cursor_placement(
                Some(saved(row, 0, size(80, 24))),
                Some(size(80, 24)),
            );
            assert_eq!(
                placement,
                ExitCursorPlacement::SavedPosition { row, col: 0 },
                "row {row}"
            );
        }
    }

    #[test]
    fn exit_placement_falls_back_when_only_height_changed_since_capture() {
        // Same width, taller screen: the saved row stays within bounds, but
        // reflow may still have moved the content, so the size-equality gate
        // must reject it and use the bottom row of the new screen.
        let placement =
            compute_exit_cursor_placement(Some(saved(5, 10, size(80, 24))), Some(size(80, 30)));
        assert_eq!(
            placement,
            ExitCursorPlacement::BottomNewline {
                bottom_row: Some(29)
            }
        );
    }

    #[test]
    fn exit_placement_falls_back_when_saved_row_is_zero_or_below_screen() {
        for row in [0, 24] {
            // Row 0: prior content can reach the bottom of the screen, so
            // nothing below the saved position is safe to overwrite. Row 24:
            // the terminal shrank while the TUI ran.
            let placement = compute_exit_cursor_placement(
                Some(saved(row, 0, size(80, 24))),
                Some(size(80, 24)),
            );
            assert_eq!(
                placement,
                ExitCursorPlacement::BottomNewline {
                    bottom_row: Some(23)
                },
                "row {row}"
            );
        }
    }

    #[test]
    fn exit_placement_uses_saved_column_only_before_rightmost_cell() {
        assert_eq!(
            compute_exit_cursor_placement(Some(saved(5, 78, size(80, 24))), Some(size(80, 24)),),
            ExitCursorPlacement::SavedPosition { row: 5, col: 78 }
        );
        for col in [79, 80, 200] {
            assert_eq!(
                compute_exit_cursor_placement(
                    Some(saved(5, col, size(80, 24))),
                    Some(size(80, 24)),
                ),
                ExitCursorPlacement::BottomNewline {
                    bottom_row: Some(23)
                },
                "col {col}"
            );
        }
    }

    #[test]
    fn exit_placement_falls_back_at_rightmost_column_on_width_one_terminal() {
        assert_eq!(
            compute_exit_cursor_placement(Some(saved(1, 0, size(1, 2))), Some(size(1, 2))),
            ExitCursorPlacement::BottomNewline {
                bottom_row: Some(1)
            }
        );
    }

    #[test]
    fn exit_placement_falls_back_when_saved_position_unknown() {
        let placement = compute_exit_cursor_placement(None, Some(size(80, 24)));
        assert_eq!(
            placement,
            ExitCursorPlacement::BottomNewline {
                bottom_row: Some(23)
            }
        );
    }

    #[test]
    fn exit_placement_falls_back_when_capture_size_unknown() {
        let placement = compute_exit_cursor_placement(
            Some(SavedCursor {
                position: position(5, 10),
                size: None,
            }),
            Some(size(80, 24)),
        );
        assert_eq!(
            placement,
            ExitCursorPlacement::BottomNewline {
                bottom_row: Some(23)
            }
        );
    }

    #[test]
    fn exit_placement_falls_back_when_height_is_one() {
        // Only row 0 exists; there is no room below any saved content.
        let placement =
            compute_exit_cursor_placement(Some(saved(0, 0, size(80, 1))), Some(size(80, 1)));
        assert_eq!(
            placement,
            ExitCursorPlacement::BottomNewline {
                bottom_row: Some(0)
            }
        );
    }

    #[test]
    fn exit_placement_uses_clamped_fallback_when_size_unknown() {
        assert_eq!(
            compute_exit_cursor_placement(Some(saved(5, 10, size(80, 24))), None),
            ExitCursorPlacement::BottomNewline { bottom_row: None }
        );
    }

    #[test]
    fn exit_placement_uses_clamped_fallback_for_zero_width_or_height() {
        // Independently zero: each alone must make the size unusable.
        assert_eq!(
            compute_exit_cursor_placement(Some(saved(5, 10, size(0, 24))), Some(size(0, 24))),
            ExitCursorPlacement::BottomNewline { bottom_row: None },
            "zero width"
        );
        assert_eq!(
            compute_exit_cursor_placement(Some(saved(5, 10, size(80, 0))), Some(size(80, 0))),
            ExitCursorPlacement::BottomNewline { bottom_row: None },
            "zero height"
        );
    }

    // Unix-only byte assertions, for the reason given above.
    // ---- emitted sequences ----

    #[cfg(not(windows))]
    #[test]
    fn saved_position_sequences_move_then_clear_below() {
        let mut out = Vec::new();
        write_exit_cursor_sequences(
            &mut out,
            ExitCursorPlacement::SavedPosition { row: 5, col: 10 },
        )
        .unwrap();
        // MoveTo is 1-based, then clear from cursor down.
        assert_eq!(out, b"\x1b[6;11H\x1b[J".as_slice());
    }

    #[cfg(not(windows))]
    #[test]
    fn bottom_newline_sequence_moves_then_scrolls_one_line() {
        let mut out = Vec::new();
        write_exit_cursor_sequences(
            &mut out,
            ExitCursorPlacement::BottomNewline {
                bottom_row: Some(23),
            },
        )
        .unwrap();
        assert_eq!(out, b"\x1b[24;1H\n".as_slice());
    }

    #[cfg(not(windows))]
    #[test]
    fn bottom_newline_without_size_clamps_row_to_last() {
        let mut out = Vec::new();
        write_exit_cursor_sequences(
            &mut out,
            ExitCursorPlacement::BottomNewline { bottom_row: None },
        )
        .unwrap();
        assert_eq!(out, b"\x1b[9999;1H\n".as_slice());
    }

    // Unix-only byte assertions, for the reason given above.
    // ---- alternate-screen entry: capture ordering and sequences ----

    #[cfg(not(windows))]
    #[test]
    fn enter_modes_captures_cursor_before_entering_alternate_screen() {
        let mut out = Vec::new();
        enter_alternate_screen_modes(
            &mut out,
            // The capture seam writes a marker so the byte stream proves the
            // capture ran before the alternate screen hid the normal screen.
            |writer| writer.write_all(b"<CAPTURE>").unwrap(),
            false,
        )
        .unwrap();
        let expected: Vec<u8> = b"<CAPTURE>\x1b[?1049h"
            .iter()
            .chain(MOUSE_CAPTURE)
            .chain(b"\x1b[?2004h")
            .copied()
            .collect();
        assert_eq!(out, expected);
    }

    #[cfg(not(windows))]
    #[test]
    fn enter_modes_pushes_keyboard_flags_between_enter_and_mouse() {
        let mut out = Vec::new();
        enter_alternate_screen_modes(&mut out, |_| {}, true).unwrap();
        let expected: Vec<u8> = b"\x1b[?1049h\x1b[>3u"
            .iter()
            .chain(MOUSE_CAPTURE)
            .chain(b"\x1b[?2004h")
            .copied()
            .collect();
        assert_eq!(out, expected);
    }

    /// Dropping the guard while a panic is unwinding must not call
    /// `panic::set_hook` (which aborts with "cannot modify the panic hook from
    /// a panicking thread"). The original panic should propagate to
    /// `catch_unwind` and the process must survive — reaching the assertion
    /// at all proves we did not abort during cleanup.
    #[test]
    fn guard_drop_during_panic_does_not_double_panic() {
        // Installs and mutates the process-global panic hook, and its guard
        // deliberately leaves that hook in place when dropped mid-unwind
        // (restoring it there would abort). Under `cargo test` that leaked hook
        // bleeds into every later panic in the shared process and destabilizes
        // sibling tests; nextest's per-test process isolation contains it.
        harnx_core::require_nextest();
        let result = std::panic::catch_unwind(|| {
            let _guard = PanicTerminalHookGuard::install();
            panic!("boom");
        });
        assert!(result.is_err(), "the original panic should propagate");
    }
}
