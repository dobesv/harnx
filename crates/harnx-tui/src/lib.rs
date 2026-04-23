//! harnx-tui — interactive terminal UI front-end for the harnx agent
//! harness (plan P49, β+ progressive peel finale). Extracted from
//! `harnx::tui`. Binds ratatui-based rendering to harnx-runtime's
//! Config/Input/Client/tool and dispatches via AgentEventSink.

pub mod agent_event_sink;
pub mod event_source;
pub mod input;
pub mod lifecycle;
pub mod prompt;
pub mod render;
pub mod render_helpers;
pub mod terminal;
pub mod test_utils;
pub mod types;

#[cfg(test)]
mod tests;

pub use types::Tui;

/// Strip ANSI escape sequences from a string for safe display in the TUI.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if c.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if c == '\x07' || c == '\x1b' {
                            break;
                        }
                    }
                }
                _ => {}
            }
        } else {
            out.push(ch);
        }
    }
    out
}
