use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use crate::repl::input_queue::InputQueue;

pub type AbortSignal = Arc<AbortSignalInner>;

pub struct AbortSignalInner {
    ctrlc: AtomicBool,
    ctrld: AtomicBool,
}

pub fn create_abort_signal() -> AbortSignal {
    AbortSignalInner::new()
}

impl AbortSignalInner {
    pub fn new() -> AbortSignal {
        Arc::new(Self {
            ctrlc: AtomicBool::new(false),
            ctrld: AtomicBool::new(false),
        })
    }

    pub fn aborted(&self) -> bool {
        if self.aborted_ctrlc() {
            return true;
        }
        if self.aborted_ctrld() {
            return true;
        }
        false
    }

    pub fn aborted_ctrlc(&self) -> bool {
        self.ctrlc.load(Ordering::SeqCst)
    }

    pub fn aborted_ctrld(&self) -> bool {
        self.ctrld.load(Ordering::SeqCst)
    }

    pub fn reset(&self) {
        self.ctrlc.store(false, Ordering::SeqCst);
        self.ctrld.store(false, Ordering::SeqCst);
    }

    pub fn set_ctrlc(&self) {
        self.ctrlc.store(true, Ordering::SeqCst);
    }

    pub fn set_ctrld(&self) {
        self.ctrld.store(true, Ordering::SeqCst);
    }
}

pub async fn wait_abort_signal(abort_signal: &AbortSignal) {
    loop {
        if abort_signal.aborted() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

pub fn poll_abort_signal(abort_signal: &AbortSignal) -> Result<bool> {
    poll_abort_signal_inner(abort_signal, None)
}

pub fn poll_abort_signal_with_input(
    abort_signal: &AbortSignal,
    input_queue: &InputQueue,
) -> Result<bool> {
    poll_abort_signal_inner(abort_signal, Some(input_queue))
}

fn poll_abort_signal_inner(
    abort_signal: &AbortSignal,
    input_queue: Option<&InputQueue>,
) -> Result<bool> {
    if crossterm::event::poll(Duration::from_millis(25))? {
        if let Event::Key(key) = event::read()? {
            if handle_abort_key(abort_signal, &key) {
                return Ok(true);
            }
            if let Some(iq) = input_queue {
                iq.handle_key_event(key);
            }
        }
    }
    Ok(false)
}

fn handle_abort_key(abort_signal: &AbortSignal, key: &KeyEvent) -> bool {
    match key.code {
        KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
            abort_signal.set_ctrlc();
            true
        }
        KeyCode::Char('d') if key.modifiers == KeyModifiers::CONTROL => {
            abort_signal.set_ctrld();
            true
        }
        _ => false,
    }
}
