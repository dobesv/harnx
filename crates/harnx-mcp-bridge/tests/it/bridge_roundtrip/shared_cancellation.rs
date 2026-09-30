use super::*;
use harnx_toolset::{ToolInvokeError, Toolset};
use tokio_util::sync::CancellationToken;

// This peer ignores cancellation and deliberately sends a late reply. Protocol
// probes are barriers: no sleep or assumption about child scheduling is needed.
const PEER: &str = r#"
import json, sys
held = None
started = []
cancelled = False
cancel_probes = []
def reply(id, result):
    print(json.dumps({'jsonrpc': '2.0', 'id': id, 'result': result}), flush=True)
def text(id, value):
    reply(id, {'content': [{'type': 'text', 'text': value}]})
for line in sys.stdin:
    req = json.loads(line)
    method = req.get('method')
    id = req.get('id')
    if method == 'initialize':
        reply(id, {'protocolVersion': req['params']['protocolVersion'], 'capabilities': {'tools': {}}, 'serverInfo': {'name': 'barrier-peer', 'version': '1'}})
    elif method == 'tools/list':
        reply(id, {'tools': [{'name': 'echo', 'description': 'barrier peer', 'inputSchema': {'type': 'object'}}]})
    elif method == 'notifications/cancelled':
        assert req['params']['requestId'] == held
        cancelled = True
        for probe in cancel_probes: text(probe, 'cancel-seen')
        cancel_probes.clear()
    elif method == 'tools/call':
        phase = req['params']['arguments']['phase']
        if phase == 'hold':
            held = id
            for probe in started: text(probe, 'started')
            started.clear()
        elif phase == 'started':
            if held is None: started.append(id)
            else: text(id, 'started')
        elif phase == 'cancel-seen':
            if cancelled: text(id, 'cancel-seen')
            else: cancel_probes.append(id)
        elif phase == 'release':
            assert cancelled
            text(held, 'late-first-reply')
            text(id, 'second-reply')
        else: text(id, 'third-reply')
"#;

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_shared_mcp_call_drops_waiter_without_disrupting_another_call() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let script = dir.path().join("barrier_peer.py");
    std::fs::write(&script, PEER)?;
    let bridge = Arc::new(
        BridgeToolset::new(
            "shared",
            vec!["python3".into(), "-u".into(), script.display().to_string()],
        )
        .await?,
    );
    let pid = bridge.child_id();
    let cancel = CancellationToken::new();
    let cleanup = harnx_toolset::cleanup::InvocationCleanup::default();
    let first = tokio::spawn({
        let bridge = bridge.clone();
        let cancel = cancel.clone();
        let cleanup = cleanup.clone();
        async move {
            harnx_toolset::cleanup::INVOCATION_CLEANUP
                .scope(
                    cleanup,
                    bridge.invoke("echo", serde_json::json!({"phase": "hold"}), cancel),
                )
                .await
        }
    });
    probe(&bridge, "started").await?;
    cancel.cancel();
    // v3 kept this call pending forever. v4 returns control independently of
    // remote shutdown, recorded as Unconfirmed rather than invented confirmation.
    let result = tokio::time::timeout(Duration::from_secs(2), first).await??;
    assert!(matches!(result, Err(ToolInvokeError::Recoverable(_))));
    assert!(cleanup
        .last_error()
        .unwrap()
        .contains("remote shutdown not confirmed"));
    probe(&bridge, "cancel-seen").await?;
    assert_eq!(
        probe(&bridge, "release").await?["content"][0]["text"],
        "second-reply"
    );
    assert_eq!(
        probe(&bridge, "third").await?["content"][0]["text"],
        "third-reply"
    );
    assert_eq!(bridge.child_id(), pid);
    assert!(!bridge.child_died_token().is_cancelled());
    Ok(())
}

async fn probe(bridge: &BridgeToolset, phase: &str) -> Result<serde_json::Value> {
    Ok(tokio::time::timeout(
        Duration::from_secs(5),
        bridge.invoke(
            "echo",
            serde_json::json!({"phase": phase}),
            CancellationToken::new(),
        ),
    )
    .await??)
}
