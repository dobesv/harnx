//! `WorkingMode` — the top-level operating mode selected at launch time
//! (one-shot CLI, TUI, HTTP server).

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum WorkingMode {
    Cmd,
    Tui,
    Serve,
}

impl WorkingMode {
    pub fn is_cmd(&self) -> bool {
        matches!(self, WorkingMode::Cmd)
    }
    pub fn is_tui(&self) -> bool {
        matches!(self, WorkingMode::Tui)
    }
    pub fn is_serve(&self) -> bool {
        matches!(self, WorkingMode::Serve)
    }
}
