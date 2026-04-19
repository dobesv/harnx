//! Terminal-polling helper for the abort signal. The `AbortSignal` type
//! itself lives in `harnx-core`; this file holds the crossterm-dependent
//! polling code that belongs to the TUI frontend (it will move to
//! `harnx-tui` in a later step).

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use std::time::Duration;

use harnx_core::abort::AbortSignal;

pub fn poll_abort_signal(abort_signal: &AbortSignal) -> Result<bool> {
    if crossterm::event::poll(Duration::from_millis(25))? {
        if let Event::Key(key) = event::read()? {
            match key.code {
                KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
                    abort_signal.set_ctrlc();
                    return Ok(true);
                }
                KeyCode::Char('d') if key.modifiers == KeyModifiers::CONTROL => {
                    abort_signal.set_ctrld();
                    return Ok(true);
                }
                _ => {}
            }
        }
    }
    Ok(false)
}
