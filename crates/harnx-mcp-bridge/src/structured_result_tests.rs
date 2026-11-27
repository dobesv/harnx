//! The structured-result request and oversized script-style results, over an
//! in-memory MCP hop with a fake child.
use super::mrtr_tests::bridge_over_server;
use super::*;
use harnx_toolset::{ToolInvocation, ToolInvocationContext, Toolset};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ListToolsResult,
    PaginatedRequestParams, ServerConfig,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::ServerHandler;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

/// Records whether each call carried the structured-result request and answers
/// with a script-style result whose text is `text_bytes` long.
struct FakeChild {
    requested: Arc<Mutex<Vec<bool>>>,
    text_bytes: usize,
}

impl ServerHandler for FakeChild {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build(),
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, rmcp::model::ErrorData> {
        Ok(ListToolsResult::with_all_items(vec![
            rmcp::model::Tool::new("cat", "Test tool", serde_json::Map::new()),
        ]))
    }

    async fn call_tool(
        &self,
        _request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, rmcp::model::ErrorData> {
        let requested = context
            .meta
            .get(harnx_toolset::CAPABILITY_STRUCTURED_RESULT)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        self.requested.lock().expect("child state").push(requested);
        let mut result = CallToolResult::success(vec![]);
        result.structured_content = Some(serde_json::json!({
            "text": "x".repeat(self.text_bytes),
        }));
        Ok(result.into())
    }
}

async fn bridge_with_child(text_bytes: usize) -> (BridgeToolset, Arc<Mutex<Vec<bool>>>) {
    let requested = Arc::new(Mutex::new(Vec::new()));
    let child = FakeChild {
        requested: requested.clone(),
        text_bytes,
    };
    (bridge_over_server(child).await, requested)
}

/// `invoke_with_context` forwards the context's request; plain `invoke` never
/// asks for one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bridge_forwards_the_structured_result_request_only_when_asked() {
    let (bridge, requested) = bridge_with_child(10).await;
    let context = ToolInvocationContext {
        capabilities: [harnx_toolset::CAPABILITY_STRUCTURED_RESULT.to_owned()].into(),
        ..Default::default()
    };
    for context in [context, ToolInvocationContext::default()] {
        bridge
            .invoke_with_context(ToolInvocation {
                tool: "cat".into(),
                args: serde_json::json!({}),
                context,
                cancel: CancellationToken::new(),
            })
            .await
            .expect("call succeeds");
    }
    bridge
        .invoke("cat", serde_json::json!({}), CancellationToken::new())
        .await
        .expect("call succeeds");
    assert_eq!(
        *requested.lock().expect("child state"),
        [true, false, false]
    );
}

/// A script-style result's data lives in `structuredContent`, which cannot be
/// offloaded to an attachment, so an oversized one is an explicit error that
/// says how to get under the limit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bridged_script_result_over_the_limit_is_a_clear_error() {
    let (bridge, _requested) = bridge_with_child(200 * 1024).await;
    let result = bridge
        .invoke("cat", serde_json::json!({}), CancellationToken::new())
        .await
        .expect("an oversized result is a recoverable error, not a failure");
    assert_eq!(result["isError"], true, "{result}");
    assert!(result.get("structuredContent").is_none());
    let text = result["content"][0]["text"].as_str().expect("error text");
    assert!(text.contains("exceeds max-result-bytes"), "{text}");
    for way_out in ["--max-output-bytes", "--limit", "--max-result-bytes"] {
        assert!(text.contains(way_out), "{way_out} missing from: {text}");
    }
}

/// A full default 128 KiB page is already over the bridge's limit, because
/// the limit counts the serialized envelope, not just the page text (#2984
/// is still open): a script must pass a page size under the cap, which the
/// error's advice names.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_default_128_kib_page_is_over_the_bridge_limit() {
    let (bridge, requested) = bridge_with_child(131_072).await;
    let result = bridge
        .invoke_with_context(ToolInvocation {
            tool: "cat".into(),
            args: serde_json::json!({}),
            context: ToolInvocationContext {
                capabilities: [harnx_toolset::CAPABILITY_STRUCTURED_RESULT.to_owned()].into(),
                ..Default::default()
            },
            cancel: CancellationToken::new(),
        })
        .await
        .expect("the call itself succeeds");
    assert_eq!(*requested.lock().expect("child state"), [true]);
    assert_eq!(result["isError"], true, "{result}");
    assert!(result.get("structuredContent").is_none());
    let text = result["content"][0]["text"].as_str().expect("error text");
    assert!(text.contains("exceeds max-result-bytes"), "{text}");
    assert!(text.contains("--max-output-bytes"), "{text}");
}
