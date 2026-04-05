use super::*;

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
        disable_raw_mode()?;
        terminal.backend_mut().execute(DisableMouseCapture)?;
        terminal.backend_mut().execute(LeaveAlternateScreen)?;
        terminal.show_cursor()?;
        Ok(())
    }
}
