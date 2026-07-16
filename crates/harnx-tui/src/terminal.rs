use crate::types::Tui;
use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders};
use ratatui::Terminal;
use ratatui_textarea::TextArea;
use std::io::{self, Stdout, Write};
use std::panic;

pub(super) fn cleanup_terminal_state() {
    let _ = disable_raw_mode();
    let mut stdout = io::stdout();
    if supports_keyboard_enhancement().unwrap_or(false) {
        let _ = stdout.execute(PopKeyboardEnhancementFlags);
    }
    let _ = stdout.execute(DisableBracketedPaste);
    let _ = stdout.execute(DisableMouseCapture);
    let _ = stdout.execute(crossterm::terminal::SetTitle("harnx"));
    let _ = stdout.execute(LeaveAlternateScreen);
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

    pub(super) fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        stdout.execute(EnterAlternateScreen)?;
        if supports_keyboard_enhancement()? {
            stdout.execute(PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_EVENT_TYPES,
            ))?;
        }
        stdout.execute(EnableMouseCapture)?;
        stdout.execute(EnableBracketedPaste)?;
        let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        Ok(terminal)
    }

    pub(super) fn restore_terminal(
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<()> {
        cleanup_terminal_state();
        terminal.show_cursor()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dropping the guard while a panic is unwinding must not call
    /// `panic::set_hook` (which aborts with "cannot modify the panic hook from a
    /// panicking thread"). The original panic should propagate to `catch_unwind`
    /// and the process must survive — reaching the assertion at all proves we did
    /// not abort during cleanup.
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
