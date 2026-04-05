use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct InputQueue(Arc<Mutex<InputQueueInner>>);

struct InputQueueInner {
    buffer: String,
    queued_message: Option<String>,
}

#[allow(dead_code)]
impl InputQueue {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(InputQueueInner {
            buffer: String::new(),
            queued_message: None,
        })))
    }

    /// Enqueue the current buffer as a message (called when Enter is pressed).
    pub fn enqueue(&self) {
        let mut inner = self.0.lock().unwrap();
        if !inner.buffer.is_empty() {
            inner.queued_message = Some(inner.buffer.clone());
        }
    }

    /// Dequeue and return the queued message, clearing it.
    pub fn dequeue(&self) -> Option<String> {
        let mut inner = self.0.lock().unwrap();
        let msg = inner.queued_message.take();
        if msg.is_some() {
            inner.buffer.clear();
        }
        msg
    }

    /// Check if a message is currently queued.
    pub fn is_queued(&self) -> bool {
        self.0.lock().unwrap().queued_message.is_some()
    }

    /// Handle a key event from crossterm during LLM processing.
    pub fn handle_key_event(&self, key: KeyEvent) {
        // Don't handle any Ctrl/Alt modified keys here (those are for abort signals)
        if key.modifiers.contains(KeyModifiers::CONTROL)
            || key.modifiers.contains(KeyModifiers::ALT)
        {
            return;
        }

        let mut inner = self.0.lock().unwrap();
        match key.code {
            KeyCode::Char(c) => {
                // If a message was queued, un-queue it (back to editing)
                inner.queued_message = None;
                inner.buffer.push(c);
            }
            KeyCode::Backspace => {
                if inner.queued_message.is_some() {
                    // Un-queue: go back to editing
                    inner.queued_message = None;
                }
                inner.buffer.pop();
            }
            KeyCode::Enter => {
                if !inner.buffer.is_empty() {
                    inner.queued_message = Some(inner.buffer.clone());
                }
            }
            KeyCode::Esc => {
                inner.buffer.clear();
                inner.queued_message = None;
            }
            _ => {}
        }
    }

    /// Get the display line for rendering below spinner/stream.
    /// Always returns a visible prompt marker so users can tell they can type.
    pub fn get_display_line(&self) -> String {
        let inner = self.0.lock().unwrap();
        if inner.queued_message.is_some() {
            format!("⏳ {}", inner.buffer)
        } else {
            format!("> {}", inner.buffer)
        }
    }

    /// Clear both the buffer and any queued message.
    pub fn clear(&self) {
        let mut inner = self.0.lock().unwrap();
        inner.buffer.clear();
        inner.queued_message = None;
    }
}
