use anyhow::Result;
use crossterm::event::{self, Event};
use std::time::Duration;

#[cfg(test)]
use crate::tui::types::TuiEvent;
#[cfg(test)]
use tokio::sync::mpsc;

pub(super) trait EventSource {
    fn poll(&mut self, timeout: Duration) -> Result<bool>;
    fn read(&mut self) -> Result<Event>;
}

pub(super) struct CrosstermEventSource;

impl EventSource for CrosstermEventSource {
    fn poll(&mut self, timeout: Duration) -> Result<bool> {
        Ok(event::poll(timeout)?)
    }

    fn read(&mut self) -> Result<Event> {
        Ok(event::read()?)
    }
}

#[cfg(test)]
pub(crate) struct MockEventSource {
    #[allow(dead_code)]
    rx: mpsc::UnboundedReceiver<TuiEvent>,
}

#[cfg(test)]
#[allow(dead_code)]
impl MockEventSource {
    pub(crate) fn new(rx: mpsc::UnboundedReceiver<TuiEvent>) -> Self {
        Self { rx }
    }
}

#[cfg(test)]
impl EventSource for MockEventSource {
    fn poll(&mut self, _timeout: Duration) -> Result<bool> {
        Ok(!self.rx.is_empty())
    }

    fn read(&mut self) -> Result<Event> {
        match self.rx.try_recv() {
            Ok(TuiEvent::UiOutput(text)) => Ok(Event::Paste(text)),
            Ok(TuiEvent::Chunk(text)) => Ok(Event::Paste(text)),
            Ok(TuiEvent::Errored(text)) => Ok(Event::Paste(text)),
            Ok(TuiEvent::ToolRoundComplete { .. }) => Ok(Event::FocusGained),
            Ok(TuiEvent::Finished { .. }) => Ok(Event::FocusGained),
            Err(err) => Err(err.into()),
        }
    }
}
