use super::*;

#[derive(Default)]
pub(super) struct RecordingCaller {
    pub(super) calls: Mutex<Vec<RecordedCall>>,
    pub(super) disconnected: Mutex<Vec<String>>,
    pub(super) responses: Mutex<VecDeque<Result<Value, McpCallError>>>,
    pub(super) wait_for_cancellation: AtomicBool,
    pub(super) call_started: Notify,
    pub(super) cancellation_observed: AtomicBool,
    pub(super) hold_after_cancellation: AtomicBool,
    pub(super) finish_cancelled_call: Notify,
}

pub(super) struct RecordedCall {
    pub(super) sandbox_id: String,
    pub(super) endpoint: String,
    pub(super) tool: String,
    pub(super) args: Map<String, Value>,
    pub(super) capabilities: BTreeSet<String>,
}

#[async_trait]
impl McpCaller for RecordingCaller {
    async fn call(
        &self,
        sandbox_id: &str,
        endpoint: &str,
        tool: &str,
        args: Map<String, Value>,
        capabilities: BTreeSet<String>,
        cancel: CancellationToken,
    ) -> Result<Value, McpCallError> {
        self.calls.lock().push(RecordedCall {
            sandbox_id: sandbox_id.to_string(),
            endpoint: endpoint.to_string(),
            tool: tool.to_string(),
            args,
            capabilities,
        });
        self.call_started.notify_one();
        if self.wait_for_cancellation.load(Ordering::SeqCst) {
            cancel.cancelled().await;
            self.cancellation_observed.store(true, Ordering::SeqCst);
            if self.hold_after_cancellation.load(Ordering::SeqCst) {
                self.finish_cancelled_call.notified().await;
            }
            return Err(McpCallError::cancelled("response", 1));
        }
        self.responses
            .lock()
            .pop_front()
            .unwrap_or_else(|| Ok(json!({"content": [{"type": "text", "text": "proxied"}]})))
    }

    async fn disconnect(&self, sandbox_id: &str) {
        self.disconnected.lock().push(sandbox_id.to_string());
    }
}
