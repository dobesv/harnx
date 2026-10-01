//! Partial results: what a call that does not succeed still hands back.

use crate::ToolInvocationContext;
use async_trait::async_trait;
use serde_json::Value;

/// Largest partial result, serialized, a tool may record. It is copied into
/// every non-success output written for the call, so it has to stay small.
pub const PARTIAL_RESULT_MAX_BYTES: usize = 4 * 1024;

/// Durable storage for one call's partial result.
#[async_trait]
pub trait PartialResultStore: Send + Sync {
    /// Replace the call's partial result. A value that arrives after the
    /// call's reply was recorded is ignored.
    async fn record_partial_result(&self, value: Value) -> anyhow::Result<()>;
}

impl ToolInvocationContext {
    /// Record what this call has produced so far. If the call then fails,
    /// times out, is interrupted or loses its response, the runtime adds the
    /// latest value to the output it writes for the call, under
    /// `partial_result`. A successful call returns only its own result.
    ///
    /// Does nothing when the transport has no store for it.
    pub async fn record_partial_result(&self, value: Value) -> anyhow::Result<()> {
        let Some(store) = &self.partial_result_store else {
            return Ok(());
        };
        let size = serde_json::to_vec(&value)?.len();
        anyhow::ensure!(
            size <= PARTIAL_RESULT_MAX_BYTES,
            "partial result is {size} bytes, over the {PARTIAL_RESULT_MAX_BYTES}-byte limit"
        );
        store.record_partial_result(value).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    /// Keeps every value a call records, in order.
    #[derive(Default)]
    struct Recorded(Mutex<Vec<Value>>);

    #[async_trait]
    impl PartialResultStore for Recorded {
        async fn record_partial_result(&self, value: Value) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(value);
            Ok(())
        }
    }

    fn context_with(store: &Arc<Recorded>) -> ToolInvocationContext {
        ToolInvocationContext {
            partial_result_store: Some(store.clone()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_partial_result_reaches_the_store() {
        let store = Arc::new(Recorded::default());

        context_with(&store)
            .record_partial_result(json!({"job": "j-1"}))
            .await
            .unwrap();

        assert_eq!(*store.0.lock().unwrap(), vec![json!({"job": "j-1"})]);
    }

    #[tokio::test]
    async fn without_a_store_recording_does_nothing() {
        ToolInvocationContext::default()
            .record_partial_result(json!({"job": "j-1"}))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn an_oversized_partial_result_never_reaches_the_store() {
        let store = Arc::new(Recorded::default());
        let oversized = json!({"blob": "x".repeat(PARTIAL_RESULT_MAX_BYTES)});

        let error = context_with(&store)
            .record_partial_result(oversized)
            .await
            .expect_err("a partial result over the limit is rejected");

        assert!(error.to_string().contains("partial result"), "{error:#}");
        assert!(store.0.lock().unwrap().is_empty());
    }
}
