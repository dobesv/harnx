use super::*;
use async_trait::async_trait;
use harnx_toolset::{ToolInvokeError, ToolSpec, Toolset};
use harnx_toolset_server::serve_over_nats;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;

pub(super) const SIBLING_BYTES: usize = 64 * 1024;
pub(super) const NATIVE_BYTES: usize = 2 * 1024 * 1024;

pub(super) fn bridge_text() -> String {
    format!("bridge START {} bridge END", "single-line-".repeat(20_000))
}

pub(super) fn native_output(tool: &str) -> Value {
    json!({"output": if tool == "native_large" { "n".repeat(NATIVE_BYTES) } else { "s".repeat(SIBLING_BYTES) }})
}

#[derive(Clone, Default)]
struct NativeTools(Arc<AtomicUsize>);

#[async_trait]
impl Toolset for NativeTools {
    fn name(&self) -> &str {
        "bounded"
    }

    fn tools(&self) -> Vec<ToolSpec> {
        ["native_large", "native_sibling"]
            .into_iter()
            .map(|name| ToolSpec {
                name: name.into(),
                description: "Return regression fixture output".into(),
                input_schema: json!({"type": "object"}),
                cancellation_guarantee: Default::default(),
                idempotent_hint: true,
                read_only_hint: true,
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
        Ok(native_output(tool))
    }
}

pub(super) struct Fixture {
    pub bridge: McpBridge,
    _native: AbortOnDropHandle<Result<()>>,
    pub calls: Arc<AtomicUsize>,
    pub env: ResultEnvironment,
}

impl Fixture {
    pub async fn start() -> Result<Self> {
        let env = ResultEnvironment::start().await?;
        let tools = NativeTools::default();
        let calls = tools.0.clone();
        let (url, scope) = (env.broker.url().to_string(), env.scope.clone());
        let native = AbortOnDropHandle::new(tokio::spawn(async move {
            serve_over_nats(tools, scope, &url, "").await
        }));
        let mut bridge = McpBridge::start(
            &env,
            BridgeScript {
                server_name: "external",
                tool_name: "bridge_large",
                result: json!({
                    "content": [
                        {"type": "text", "text": bridge_text()},
                        {"type": "text", "text": "fitting MCP part"}
                    ],
                    "isError": false
                }),
            },
        )
        .await?;
        bridge
            .wait_for_servers(&env, &["bounded", "external"])
            .await?;
        Ok(Self {
            bridge,
            _native: native,
            calls,
            env,
        })
    }

    pub async fn session(&self, api_base: &str) -> Result<ProviderSession> {
        ProviderSession::create(
            &self.env,
            SessionSpec {
                id: "bounds123",
                api_base,
                supports_vision: false,
            },
        )
        .await
    }

    pub async fn stop(self) -> Result<()> {
        self.bridge.stop().await
    }
}
