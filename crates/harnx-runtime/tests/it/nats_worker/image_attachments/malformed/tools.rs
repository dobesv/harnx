use super::*;
use async_trait::async_trait;
use harnx_toolset::{ToolInvokeError, ToolSpec, Toolset};
use tokio_util::sync::CancellationToken;

pub(super) fn malformed_output() -> Value {
    json!({"content": [
        {"type": "text", "text": "original tool diagnostic, side effect completed"},
        {"type": "resource_link", "uri": "cid:media:invalid", "mimeType": "image/png"}
    ], "isError": false, "structuredContent": {"diagnostic": 17}})
}

struct NativeTools(Arc<AtomicUsize>);

#[async_trait]
impl Toolset for NativeTools {
    fn name(&self) -> &str {
        "malformed"
    }

    fn tools(&self) -> Vec<ToolSpec> {
        ["malformed", "sibling"]
            .into_iter()
            .map(|name| ToolSpec {
                name: name.into(),
                description: "Return regression fixture output".into(),
                input_schema: json!({"type": "object"}),
                cancellation_guarantee: Default::default(),
                idempotent_hint: false,
                read_only_hint: false,
                timeout_secs: None,
                meta: None,
            })
            .collect()
    }

    async fn invoke(
        &self,
        tool: &str,
        _: Value,
        _: CancellationToken,
    ) -> Result<Value, ToolInvokeError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(if tool == "malformed" {
            malformed_output()
        } else {
            json!({"sibling": "retained"})
        })
    }
}

pub(super) fn start(
    env: &ResultEnvironment,
    calls: Arc<AtomicUsize>,
) -> AbortOnDropHandle<Result<()>> {
    let url = env.broker.url().to_string();
    let scope = env.scope.clone();
    AbortOnDropHandle::new(tokio::spawn(async move {
        harnx_toolset_server::serve_over_nats(NativeTools(calls), scope, &url, "").await
    }))
}
