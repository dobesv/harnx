use super::*;
use harnx_toolset::Toolset;
use tokio_util::sync::CancellationToken;

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_shared_mcp_call_waits_without_disrupting_another_call() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let script = dir.path().join("delayed.yaml");
    std::fs::write(
        &script,
        "tools:\n  - name: echo\nresponses: []\nfallback: finished\nresponse_delay_ms: 600\n",
    )?;
    let bridge = BridgeToolset::new(
        "shared",
        vec![
            mock_mcp_binary()?.display().to_string(),
            "--script".into(),
            script.display().to_string(),
        ],
    )
    .await?;
    let pid = bridge.child_id();
    let cancel = CancellationToken::new();
    let first = bridge.invoke("echo", serde_json::json!({}), cancel.clone());
    tokio::pin!(first);
    assert!(tokio::time::timeout(Duration::from_millis(100), &mut first)
        .await
        .is_err());
    cancel.cancel();
    // The mock handler ignores cancellation while it sleeps. Sending its
    // notification must not claim that the invocation has already stopped.
    assert!(tokio::time::timeout(Duration::from_millis(100), &mut first)
        .await
        .is_err());
    let second = bridge.invoke("echo", serde_json::json!({}), CancellationToken::new());
    let second = tokio::time::timeout(Duration::from_secs(5), second).await??;
    assert_eq!(second["content"][0]["text"], "finished");
    // RMCP's server discards the cancelled handler's eventual response. No
    // remote acknowledgement exists, so the first call must remain unconfirmed.
    assert!(tokio::time::timeout(Duration::from_millis(100), &mut first)
        .await
        .is_err());
    assert_eq!(bridge.child_id(), pid);
    assert!(!bridge.child_died_token().is_cancelled());
    Ok(())
}
