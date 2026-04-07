//! Synchronization primitives for async test coordination.
//!
//! This module provides [`SyncHarness`] which helps coordinate async tests
//! by waiting for conditions to be met with configurable timeouts and polling.
//!
//! # Example
//!
//! ```ignore
//! use harnx::test_utils::SyncHarness;
//! use std::time::Duration;
//!
//! let harness = SyncHarness::new()
//!     .with_poll_interval(Duration::from_millis(10));
//!
//! // Wait for a mock client to exhaust its turns
//! harness.wait_until_mock_exhausted(&mock, Duration::from_secs(5)).await?;
//! ```

use crate::test_utils::MockClient;

use anyhow::{anyhow, Result};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{sleep, timeout};

/// Function type for probing idle state asynchronously.
pub type IdleProbe = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = bool> + Send>> + Send + Sync>;

/// Function type for capturing diagnostic trace on timeout.
pub type TraceProbe = Arc<dyn Fn() -> String + Send + Sync>;

/// Harness for waiting on async conditions in tests.
///
/// Provides configurable polling with timeouts for coordinating
/// async test execution.
#[derive(Clone)]
pub struct SyncHarness {
    idle_probe: Option<IdleProbe>,
    trace_probe: Option<TraceProbe>,
    poll_interval: Duration,
}

impl Default for SyncHarness {
    fn default() -> Self {
        Self {
            idle_probe: None,
            trace_probe: None,
            poll_interval: Duration::from_millis(10),
        }
    }
}

impl SyncHarness {
    /// Create a new sync harness with default settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Configure an async probe to check for idle state.
    ///
    /// The probe should return `true` when the system is idle.
    pub fn with_idle_probe<F, Fut>(mut self, probe: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = bool> + Send + 'static,
    {
        self.idle_probe = Some(Arc::new(move || Box::pin(probe())));
        self
    }

    /// Configure a trace probe for diagnostics on timeout.
    pub fn with_trace_probe<F>(mut self, probe: F) -> Self
    where
        F: Fn() -> String + Send + Sync + 'static,
    {
        self.trace_probe = Some(Arc::new(probe));
        self
    }

    /// Set the polling interval for wait conditions.
    pub fn with_poll_interval(mut self, poll_interval: Duration) -> Self {
        self.poll_interval = poll_interval;
        self
    }

    /// Wait until the screen contains expected text.
    pub async fn wait_until_screen_contains<G>(
        &self,
        expected: &str,
        timeout_duration: Duration,
        mut screen: G,
    ) -> Result<()>
    where
        G: FnMut() -> String,
    {
        self.wait_until(
            format!("screen to contain {expected:?}"),
            timeout_duration,
            || {
                let contents = screen();
                contents.contains(expected)
            },
        )
        .await
    }

    /// Wait until a mock client has exhausted all its scripted turns.
    pub async fn wait_until_mock_exhausted(
        &self,
        mock_client: &MockClient,
        timeout_duration: Duration,
    ) -> Result<()> {
        self.wait_until(
            "mock client to exhaust scripted turns",
            timeout_duration,
            || mock_client.remaining_turns() == 0,
        )
        .await
    }

    /// Wait until the idle probe indicates the system is idle.
    pub async fn wait_until_idle(&self, timeout_duration: Duration) -> Result<()> {
        let Some(idle_probe) = self.idle_probe.as_ref() else {
            return Err(anyhow!("idle probe is not configured"));
        };

        self.wait_until_async("idle state", timeout_duration, || {
            let idle_probe = Arc::clone(idle_probe);
            async move { idle_probe().await }
        })
        .await
    }

    pub fn tracing_on_failure(&self) -> Option<String> {
        self.trace_probe.as_ref().map(|probe| probe())
    }

    async fn wait_until<F>(&self, label: impl Into<String>, timeout_duration: Duration, mut predicate: F) -> Result<()>
    where
        F: FnMut() -> bool,
    {
        let label = label.into();
        let poll_interval = self.poll_interval;
        timeout(timeout_duration, async move {
            loop {
                if predicate() {
                    return;
                }
                sleep(poll_interval).await;
            }
        })
        .await
        .map_err(|_| {
            let trace = self.tracing_on_failure();
            match trace {
                Some(trace) if !trace.is_empty() => anyhow!(
                    "timed out waiting for {label} after {:?}\n\nCaptured trace:\n{trace}",
                    timeout_duration
                ),
                _ => anyhow!("timed out waiting for {label} after {:?}", timeout_duration),
            }
        })?;
        Ok(())
    }

    async fn wait_until_async<F, Fut>(
        &self,
        label: impl Into<String>,
        timeout_duration: Duration,
        mut predicate: F,
    ) -> Result<()>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = bool>,
    {
        let label = label.into();
        let poll_interval = self.poll_interval;
        timeout(timeout_duration, async move {
            loop {
                if predicate().await {
                    return;
                }
                sleep(poll_interval).await;
            }
        })
        .await
        .map_err(|_| {
            let trace = self.tracing_on_failure();
            match trace {
                Some(trace) if !trace.is_empty() => anyhow!(
                    "timed out waiting for {label} after {:?}\n\nCaptured trace:\n{trace}",
                    timeout_duration
                ),
                _ => anyhow!("timed out waiting for {label} after {:?}", timeout_duration),
            }
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Client;
    use crate::test_utils::MockTurnBuilder;
    use parking_lot::RwLock;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Instant;

    #[tokio::test(flavor = "multi_thread")]
    async fn wait_until_screen_contains_blocks_until_text_is_rendered() {
        let screen = Arc::new(RwLock::new(String::new()));
        let writer = Arc::clone(&screen);
        tokio::spawn(async move {
            sleep(Duration::from_millis(40)).await;
            *writer.write() = "assistant: ready".to_string();
        });

        let harness = SyncHarness::new().with_poll_interval(Duration::from_millis(5));
        let started = Instant::now();
        harness
            .wait_until_screen_contains("ready", Duration::from_millis(250), || {
                screen.read().clone()
            })
            .await
            .unwrap();

        assert!(started.elapsed() >= Duration::from_millis(30));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn wait_until_mock_exhausted_observes_turn_consumption() {
        let client = Arc::new(
            MockClient::builder()
                .add_turn(MockTurnBuilder::new().add_text_chunk("first").build())
                .build(),
        );
        let harness = SyncHarness::new();

        assert_eq!(client.remaining_turns(), 1);

        let consumer_client = Arc::clone(&client);
        tokio::spawn(async move {
            sleep(Duration::from_millis(30)).await;
            let reqwest_client = reqwest::Client::new();
            let data = crate::client::ChatCompletionsData {
                messages: vec![],
                temperature: None,
                top_p: None,
                functions: None,
                stream: false,
            };
            let _ = consumer_client.chat_completions_inner(&reqwest_client, data).await;
        });

        harness
            .wait_until_mock_exhausted(client.as_ref(), Duration::from_millis(250))
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn wait_until_idle_uses_async_probe_and_emits_trace_on_failure() {
        let idle = Arc::new(AtomicBool::new(false));
        let idle_for_task = Arc::clone(&idle);
        tokio::spawn(async move {
            sleep(Duration::from_millis(35)).await;
            idle_for_task.store(true, Ordering::SeqCst);
        });

        let harness = SyncHarness::new()
            .with_poll_interval(Duration::from_millis(5))
            .with_idle_probe({
                let idle = Arc::clone(&idle);
                move || {
                    let idle = Arc::clone(&idle);
                    async move { idle.load(Ordering::SeqCst) }
                }
            })
            .with_trace_probe(|| "pending events: 1".to_string());

        harness
            .wait_until_idle(Duration::from_millis(250))
            .await
            .unwrap();

        let err = SyncHarness::new()
            .with_poll_interval(Duration::from_millis(5))
            .with_idle_probe(|| async { false })
            .with_trace_probe(|| "pending events: 1".to_string())
            .wait_until_idle(Duration::from_millis(25))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Captured trace"));
    }
}
