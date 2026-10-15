use super::*;
use serde_json::json;

#[test]
fn rpc_timeout_secs_parsing() {
    let omitted: PromptParams = serde_json::from_value(json!({"text": "hello"})).unwrap();
    assert_eq!(omitted.timeout_secs, None);
    for (value, expected) in [
        (json!(null), None),
        (json!(0), None),
        (json!(-1), None),
        (json!(23), Some(23)),
    ] {
        let input: PromptParams =
            serde_json::from_value(json!({"text": "hello", "timeout_secs": value})).unwrap();
        assert_eq!(input.timeout_secs, expected, "input: {value}");
    }
    for value in [
        json!(1.5),
        json!("23"),
        json!(true),
        json!([]),
        json!({}),
        json!(u64::MAX),
    ] {
        assert!(
            serde_json::from_value::<PromptParams>(json!({"text": "hello", "timeout_secs": value}))
                .is_err(),
            "invalid timeout accepted: {value}"
        );
    }
}

#[tokio::test]
async fn rpc_timeout_reaches_executor_and_cannot_extend_policy() {
    use crate::test_support::TestConfigSandbox;
    use harnx_runtime::{client::TestStateGuard, AgentCallFn};
    use std::sync::Arc;
    use tokio::time::Duration;
    let _guard = TestStateGuard::new(None).await;
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent_with_front_matter(
        "plain",
        "run_limits:\n  timeout_secs: 30",
        "You are plain.",
    );
    let config = sandbox.config();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let call_fn: AgentCallFn = Arc::new(move |_, config, _| {
        let tx = tx.clone();
        Box::pin(async move {
            tx.send(config.read().run_context.clone().expect("admission limits"))
                .unwrap();
            Ok(("done".into(), None, vec![], Default::default()))
        })
    });
    let registry =
        SessionRegistry::new_for_tests(config.clone(), Duration::from_secs(30), Some(call_fn));
    for (index, (value, expected)) in [
        (None, 30),
        (Some(json!(null)), 30),
        (Some(json!(0)), 30),
        (Some(json!(-1)), 30),
        (Some(json!(10)), 10),
        (Some(json!(30)), 30),
        (Some(json!(60)), 30),
    ]
    .into_iter()
    .enumerate()
    {
        let mut params = json!({"text": "hello"});
        if let Some(value) = value {
            params["timeout_secs"] = value;
        }
        let response = handle_ag_ui_rpc_bytes(
            Method::POST,
            "plain",
            &format!("rpc-timeout-{index}"),
            bytes::Bytes::from(
                json!({"jsonrpc":"2.0", "id":index, "method":"session/prompt", "params":params})
                    .to_string(),
            ),
            &config,
            &registry,
            PersistenceKind::Nats,
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let limits = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            limits
                .deadline
                .unwrap()
                .signed_duration_since(limits.admitted_at)
                .num_seconds(),
            expected
        );
    }
}
