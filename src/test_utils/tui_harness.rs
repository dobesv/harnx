//! Test harness for TUI rendering tests using ratatui::TestBackend.
//!
//! This module provides [`TuiTestHarness`] which allows testing TUI rendering
//! without a real terminal. It uses [`ratatui::backend::TestBackend`] to capture
//! rendered output.

use crate::config::{Config, GlobalConfig};
use crate::hooks::{AsyncHookManager, PersistentHookManager};
use crate::tui::types::Tui;
use crate::test_utils::SyncHarness;

use anyhow::Result;
use parking_lot::RwLock;
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use std::sync::Arc;
use tokio::sync::Mutex;

/// A test harness for TUI rendering tests.
///
/// This harness creates a TUI with a [`TestBackend`] that captures rendered
/// output to an in-memory buffer. The rendered output can be inspected for
/// assertions or snapshot testing.
///
/// # Example
///
/// ```ignore
/// use crate::test_utils::TuiTestHarness;
///
/// #[test]
/// fn test_basic_render() {
///     let mut harness = TuiTestHarness::new();
///     harness.render();
///     let output = harness.screen_contents();
///     assert!(output.contains("some expected text"));
/// }
/// ```
pub struct TuiTestHarness {
    tui: Tui,
    terminal: Terminal<TestBackend>,
    sync: SyncHarness,
}

impl TuiTestHarness {
    /// Create a new test harness with default configuration.
    pub fn new() -> Self {
        Self::with_size(80, 24)
    }

    /// Create a new test harness with a specific terminal size.
    pub fn with_size(width: u16, height: u16) -> Self {
        let config = Self::create_test_config();
        Self::with_config_and_size(config, width, height)
    }

    /// Create a test harness with a specific configuration.
    pub fn with_config(config: GlobalConfig) -> Self {
        Self::with_config_and_size(config, 80, 24)
    }

    fn with_config_and_size(config: GlobalConfig, width: u16, height: u16) -> Self {
        let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
        let tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();
        
        let backend = TestBackend::new(width, height);
        let terminal = Terminal::new(backend).unwrap();
        
        Self {
            tui,
            terminal,
            sync: SyncHarness::new(),
        }
    }

    fn create_test_config() -> GlobalConfig {
        Arc::new(RwLock::new(Config::default()))
    }

    /// Get the current screen contents as a string.
    pub fn screen_contents(&self) -> String {
        let buffer = self.terminal.backend().buffer();
        
        let mut contents = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                let cell = &buffer[(x, y)];
                contents.push_str(cell.symbol());
            }
            if y < buffer.area.height - 1 {
                contents.push('\n');
            }
        }
        contents
    }

    /// Render the TUI to the test backend.
    pub fn render(&mut self) {
        let _ = self.terminal.draw(|frame| self.tui.draw(frame));
    }

    /// Get a reference to the underlying TUI for direct manipulation.
    pub fn tui(&mut self) -> &mut Tui {
        &mut self.tui
    }

    /// Get a reference to the sync harness for wait conditions.
    pub fn sync(&self) -> &SyncHarness {
        &self.sync
    }
}

impl Default for TuiTestHarness {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use insta::assert_snapshot;

    #[tokio::test]
    async fn harness_renders_initial_screen() {
        let mut harness = TuiTestHarness::new();
        harness.render();
        let contents = harness.screen_contents();
        // The screen should contain something, even if just whitespace
        assert!(!contents.is_empty());
    }

    #[tokio::test]
    async fn screen_contents_snapshot() {
        let mut harness = TuiTestHarness::with_size(40, 10);
        harness.render();
        let contents = harness.screen_contents();
        // Normalize the output by trimming trailing whitespace from each line
        let normalized: String = contents
            .lines()
            .map(|line| line.trim_end())
            .collect::<Vec<_>>()
            .join("\n");
        insta::with_settings!({
            description => "Basic TUI render test"
        }, {
            assert_snapshot!("screen_contents_snapshot", normalized);
        });
    }
}