use super::*;

fn cleanup_terminal_state() {
    let _ = disable_raw_mode();
    let mut stdout = io::stdout();
    let _ = stdout.execute(DisableMouseCapture);
    let _ = stdout.execute(LeaveAlternateScreen);
    let _ = stdout.flush();
}

type PanicHook = Box<dyn Fn(&panic::PanicHookInfo<'_>) + Sync + Send + 'static>;

pub(super) struct PanicTerminalHookGuard {
    previous_hook: Option<PanicHook>,
}

impl PanicTerminalHookGuard {
    pub(super) fn install() -> Self {
        let previous_hook = panic::take_hook();
        let hook_to_restore = panic::take_hook();
        panic::set_hook(previous_hook);
        let chained_hook = panic::take_hook();
        panic::set_hook(Box::new(move |panic_info: &panic::PanicHookInfo<'_>| {
            cleanup_terminal_state();
            chained_hook(panic_info);
        }));
        Self {
            previous_hook: Some(hook_to_restore),
        }
    }
}

impl Drop for PanicTerminalHookGuard {
    fn drop(&mut self) {
        if let Some(previous_hook) = self.previous_hook.take() {
            panic::set_hook(previous_hook);
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
        // input.set_placeholder_text("Enter submits · Shift+Enter / Ctrl+J for newline");
        input
    }

    pub(super) fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        stdout.execute(EnterAlternateScreen)?;
        stdout.execute(EnableMouseCapture)?;
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
