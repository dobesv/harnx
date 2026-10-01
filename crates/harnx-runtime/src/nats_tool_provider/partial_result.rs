//! A call that fails recoverably answers with the partial result its tool
//! recorded on the call's journal row.
use super::*;
use harnx_core::partial_result::error_with_partial_result;

/// A sick broker must not hold a failed call's answer hostage to a best-effort read.
pub(super) const PARTIAL_RESULT_READ_TIMEOUT: Duration = Duration::from_secs(5);

impl NatsToolProvider {
    /// Attach the partial result the call's tool recorded when the call
    /// failed recoverably. A fatal error ends the turn without an output for
    /// the call. Whatever answers the call afterwards, wind-up after a
    /// `Cancel` or a replay, reads the row itself.
    pub(super) async fn with_recorded_partial_result(
        &self,
        request: &ToolRequest,
        result: Result<ToolProviderOutput, ToolError>,
    ) -> Result<ToolProviderOutput, ToolError> {
        if !matches!(result, Err(ToolError::Recoverable(_))) {
            return result;
        }
        let partial_result = self.recorded_partial_result(request).await;
        attach_partial_result(result, partial_result.as_ref())
    }

    /// An unreadable row costs the output its partial result, never the call
    /// its error.
    async fn recorded_partial_result(&self, request: &ToolRequest) -> Option<Value> {
        // Nothing can be read until the connection is back, and the outage is
        // already reported where the call failed, so the skip stays at debug.
        if self.client.connection_state() != async_nats::connection::State::Connected {
            log::debug!(
                "skip reading the partial result of tool call {}: NATS is not connected",
                request.call_id
            );
            return None;
        }
        let js = async_nats::jetstream::new(self.client.clone());
        let row = tokio::time::timeout(PARTIAL_RESULT_READ_TIMEOUT, async {
            harnx_toolset_server::invocation_journal::InvocationJournal::ensure(
                &js,
                self.journal_replicas,
            )
            .await?
            .get(request)
            .await
        })
        .await
        .unwrap_or_else(|_| Err(anyhow!("timed out after {PARTIAL_RESULT_READ_TIMEOUT:?}")));
        match row {
            Ok(row) => row.and_then(|row| row.partial_result),
            Err(error) => {
                log::warn!(
                    "read the partial result of tool call {}: {error:#}",
                    request.call_id
                );
                None
            }
        }
    }
}

/// Attach `partial_result` to a recoverable failure. Anything else passes
/// through unchanged.
pub(super) fn attach_partial_result(
    result: Result<ToolProviderOutput, ToolError>,
    partial_result: Option<&Value>,
) -> Result<ToolProviderOutput, ToolError> {
    match (result, partial_result) {
        (Err(ToolError::Recoverable(error)), Some(partial_result)) => Err(ToolError::Recoverable(
            error_with_partial_result(error, partial_result.clone()),
        )),
        (result, _) => result,
    }
}
