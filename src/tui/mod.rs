use crate::client::{call_chat_completions, CompletionTokenUsage};
use crate::config::{GlobalConfig, Input};
use crate::hooks::{
    dispatch_hooks_with_count_and_manager, drain_async_results, inject_pending_async_context,
    AsyncHookManager, HookEvent, HookResultControl, PersistentHookManager,
};
use crate::tool::ToolResult;
use crate::ui_output::install_ui_output_sender;
use crate::utils::{create_abort_signal, AbortSignal};

use anyhow::Result;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, KeyboardEnhancementFlags, MouseEvent, MouseEventKind,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use ratatui_textarea::{Input as TextInput, Key, TextArea};
use std::io::{self, Stdout, Write};
use std::panic;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, Mutex};

mod event_source;
mod input;
mod lifecycle;
mod prompt;
mod render;
mod terminal;
#[cfg(test)]
mod tests;
mod types;

pub use self::types::Tui;

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
