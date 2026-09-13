//! Integration test proving the key scenario from issue #1862.
//!
//! This test runs against the DIRECT `PlansServer` `ServerHandler` implementation
//! via a dedicated hermetic test binary (`harnx-mcp-plans-hermetic`). It proves
//! that domain failures return `Ok(CallToolResult { is_error: Some(true) })`
//! rather than JSON-RPC error frames, and that the connection stays alive
//! after recoverable errors.
//!
//! # Why NOT `harnx-plans-tools --mcp-stdio`?
//!
//! That path routes through `harnx_toolset_server::run_toolset_main(PlansToolset)`
//! -> `McpToolsetAdapter`, which converts handler `Err` to `isError` results at
//! the adapter layer (see `crates/harnx-toolset-server/src/lib.rs:1064-1069`).
//! A test against that binary would pass even WITHOUT the ServerHandler error-mapping fix — it
//! would exercise the adapter, not the direct handler.
//!
//! # How this test guarantees it hits the direct handler
//!
//! 1. Spawns the `harnx-mcp-plans-hermetic` binary, which directly constructs
//!    `PlansServer<InMemoryStore>` and serves it via `rmcp::transport::stdio()`.
//! 2. No `McpToolsetAdapter` is involved in this binary's code path.
//! 3. The `domain_result` wrapper in `harnx-mcp-plans-core/src/server/handler.rs`
//!    is the ONLY conversion point between handler `Err` and `isError` results.
//!
//! If this test fails (e.g., client receives an error frame), it proves the
//! direct handler was hit and the `domain_result` wrapper is missing or broken.

#![cfg(unix)]

use rmcp::handler::client::ClientHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, ContentBlock, Implementation,
    InitializeRequestParams,
};
use rmcp::service::RoleClient;
use rmcp::transport::async_rw::AsyncRwTransport;
use serde_json::json;
use std::process::Stdio;
use tokio::process::Command;

struct RawClientHandler;

impl ClientHandler for RawClientHandler {
    fn get_info(&self) -> InitializeRequestParams {
        InitializeRequestParams::new(
            ClientCapabilities::default(),
            Implementation::new("stdio-roundtrip-test", env!("CARGO_PKG_VERSION")),
        )
    }
}

/// Asserts that a known-tool domain failure returns `Ok` with `is_error: Some(true)`
/// and that the content contains the expected message fragment.
fn assert_is_error_result(
    result: &Result<CallToolResult, rmcp::service::ServiceError>,
    contains: &str,
) {
    match result {
        Ok(result) => {
            assert!(
                result.is_error == Some(true),
                "expected is_error: Some(true), got {:?}",
                result.is_error
            );
            let text = result
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect::<String>();
            assert!(
                text.contains(contains),
                "expected content to contain {:?}, got {:?}",
                contains,
                text
            );
        }
        Err(e) => panic!("expected Ok result, got Err (protocol error): {:?}", e),
    }
}

/// Asserts that a successful call returns `Ok` with `is_error != Some(true)`.
fn assert_success_result(result: &Result<CallToolResult, rmcp::service::ServiceError>) {
    match result {
        Ok(result) => {
            assert!(
                result.is_error != Some(true),
                "expected success without is_error: true, got is_error: {:?}",
                result.is_error
            );
        }
        Err(e) => panic!(
            "expected Ok success result, got Err (protocol error): {:?}",
            e
        ),
    }
}

/// Integration test proving the key scenario from issue #1862.
///
/// Sequence (all on the SAME stdio connection):
/// 1. Call `get_note` on a non-existent note -> `Ok` with `is_error: Some(true)`, content mentions the missing note ID.
/// 2. Call `add_plan` to create a plan -> success (`is_error != Some(true)`).
/// 3. Call `add_note` to create a valid note -> success.
/// 4. Call `get_note` on that valid note -> success.
/// 5. Call `add_plan` with the duplicate plan name -> `Ok` with `is_error: Some(true)`.
/// 6. Call `update_note` with `replace_in_body: { old_text: "not present in note", new_text: "foo" }` -> `Ok` with `is_error: Some(true)`.
///
/// Throughout, verify NO JSON-RPC error frames / `McpError` occur — the client stays connected.
#[tokio::test]
async fn plans_server_stdio_roundtrip_recoverable_errors() {
    // Spawn the hermetic MCP server that directly serves `PlansServer<InMemoryStore>`.
    let mut child = Command::new(env!("CARGO_BIN_EXE_harnx-mcp-plans-hermetic"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn harnx-mcp-plans-hermetic");

    let child_stdin = child.stdin.take().expect("take child stdin");
    let child_stdout = child.stdout.take().expect("take child stdout");
    let transport = AsyncRwTransport::<RoleClient, _, _>::new(child_stdout, child_stdin);
    let service = rmcp::service::serve_client(RawClientHandler, transport)
        .await
        .expect("initialize raw rmcp client");
    let peer = service.peer().clone();

    // STEP 1: Call `get_note` on a non-existent note.
    // Expected: Ok(result) with is_error: Some(true), content mentions "nonexistent-note".
    let result = peer
        .call_tool(
            CallToolRequestParams::new("get_note").with_arguments(
                json!({
                    "plan": "test-plan",
                    "note_id": "nonexistent-note"
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await;
    assert_is_error_result(&result, "nonexistent-note");
    // Connection should still be alive — no protocol error.

    // STEP 1b: Call `get_note` on a non-existent plan.
    // Expected: Ok(result) with is_error: Some(true), content mentions the plan not found.
    let result = peer
        .call_tool(
            CallToolRequestParams::new("get_note").with_arguments(
                json!({
                    "plan": "nonexistent-plan",
                    "note_id": "some-note"
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await;
    // The error should mention the plan not found.
    assert_is_error_result(&result, "not found");

    // STEP 2: Create a plan.
    // Expected: success (is_error != Some(true)).
    let result = peer
        .call_tool(
            CallToolRequestParams::new("add_plan").with_arguments(
                json!({
                    "name": "test-plan",
                    "title": "Test Plan"
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await;
    assert_success_result(&result);

    // STEP 3: Create a note in the plan.
    // Expected: success.
    let result = peer
        .call_tool(
            CallToolRequestParams::new("add_note").with_arguments(
                json!({
                    "plan": "test-plan",
                    "body": "This is the initial note body."
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await;
    assert_success_result(&result);

    // Extract the note ID from the result to use in subsequent calls.
    // Response format: "added note <id> to plan test-plan\n\n```diff\n..."
    // Parse the first line to extract the note ID.
    let note_id = match result.as_ref().unwrap().content.first() {
        Some(ContentBlock::Text(t)) => {
            // Extract ID from message like "added note abc12345 to plan test-plan"
            let first_line = t.text.lines().next().unwrap_or("");
            // ID is between "added note " and " to plan"
            let parts: Vec<&str> = first_line.split(" added note ").collect();
            if parts.len() > 1 {
                let after = parts[1];
                let id_end = after.find(" to plan").unwrap_or(after.len());
                after[..id_end].to_string()
            } else {
                // Alternate parsing: "added note <id>"
                let parts: Vec<&str> = first_line.split_whitespace().collect();
                if parts.len() >= 3 && parts[0] == "added" && parts[1] == "note" {
                    parts[2].to_string()
                } else {
                    panic!("could not parse note ID from: {:?}", first_line);
                }
            }
        }
        _ => panic!("expected text content in add_note result"),
    };

    // STEP 4: Get the valid note on the SAME connection.
    // Expected: success.
    let result = peer
        .call_tool(
            CallToolRequestParams::new("get_note").with_arguments(
                json!({
                    "plan": "test-plan",
                    "note_id": &note_id
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await;
    assert_success_result(&result);

    // STEP 5: Attempt to create a duplicate plan.
    // Expected: Ok(result) with is_error: Some(true), content mentions "already exists".
    let result = peer
        .call_tool(
            CallToolRequestParams::new("add_plan").with_arguments(
                json!({
                    "name": "test-plan",
                    "title": "Duplicate Test Plan"
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await;
    assert_is_error_result(&result, "already exists");

    // STEP 6: Attempt update_note with replace_in_body where old_text is not present.
    // Expected: Ok(result) with is_error: Some(true), content mentions "old_text not found".
    let result = peer
        .call_tool(
            CallToolRequestParams::new("update_note").with_arguments(
                json!({
                    "plan": "test-plan",
                    "note_id": &note_id,
                    "replace_in_body": {
                        "old_text": "not present in note",
                        "new_text": "foo"
                    }
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await;
    assert_is_error_result(&result, "old_text not found");

    // STEP 7: Verify the connection is still alive by making a successful call.
    let result = peer
        .call_tool(CallToolRequestParams::new("list_plans"))
        .await;
    assert_success_result(&result);

    // Cleanup: cancel the client and kill the server.
    service.cancel().await.expect("close raw rmcp client");
    child.kill().await.expect("stop harnx-mcp-plans-hermetic");
}

/// Verifies that an unknown tool returns a protocol error (Err from call_tool),
/// not an isError result. This must be the case even after domain failures.
#[tokio::test]
async fn unknown_tool_returns_protocol_error_after_domain_errors() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_harnx-mcp-plans-hermetic"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn harnx-mcp-plans-hermetic");

    let child_stdin = child.stdin.take().expect("take child stdin");
    let child_stdout = child.stdout.take().expect("take child stdout");
    let transport = AsyncRwTransport::<RoleClient, _, _>::new(child_stdout, child_stdin);
    let service = rmcp::service::serve_client(RawClientHandler, transport)
        .await
        .expect("initialize raw rmcp client");
    let peer = service.peer().clone();

    // First trigger a domain error.
    let result = peer
        .call_tool(
            CallToolRequestParams::new("get_note").with_arguments(
                json!({
                    "plan": "missing",
                    "note_id": "missing"
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await;
    assert_is_error_result(&result, "not found");

    // Now call an unknown tool.
    let result = peer
        .call_tool(CallToolRequestParams::new("unknown_tool_xyz"))
        .await;

    // Unknown tool should return Err(ServiceError), not Ok(is_error).
    match result {
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("unknown tool")
                    || msg.contains("invalid")
                    || msg.contains("InvalidParams"),
                "expected protocol error for unknown tool, got: {:?}",
                e
            );
        }
        Ok(result) => {
            panic!("expected Err for unknown tool, got Ok: {:?}", result);
        }
    }

    service.cancel().await.expect("close raw rmcp client");
    child.kill().await.expect("stop harnx-mcp-plans-hermetic");
}
