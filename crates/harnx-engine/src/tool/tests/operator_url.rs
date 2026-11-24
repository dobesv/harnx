//! Operator-origin tool challenges (decision 11): the narrow `url` exception.

use super::*;
use harnx_core::tool::ToolInputRequired;
use std::pin::Pin;

/// A provider that answers every call with a raw input-required challenge.
struct ChallengeProvider {
    challenge: ToolInputRequired,
}

// The `ToolProvider` trait is `#[async_trait]`, and this crate doesn't depend
// on the macro; hand-desugar like the sibling mocks in `tool.rs`.
impl ToolProvider for ChallengeProvider {
    fn name(&self) -> &str {
        "challenge"
    }

    fn has_tool(&self, tool_name: &str) -> bool {
        tool_name == "jira_connect"
    }

    fn call_tool<'life0, 'life1, 'life2, 'async_trait>(
        &'life0 self,
        _tool_name: &'life1 str,
        _arguments: Value,
        _abort: &'life2 AbortSignal,
    ) -> Pin<Box<dyn Future<Output = Result<ToolProviderOutput, ToolError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        'life2: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            Ok(ToolProviderOutput {
                value: Value::Null,
                execution_context: None,
                input_required: Some(self.challenge.clone()),
            })
        })
    }
}

fn url_challenge() -> ToolInputRequired {
    ToolInputRequired {
        producer: "tool:jira/connect".into(),
        key: "connect".into(),
        kind: "url".into(),
        message: Some("Connect Jira".into()),
        surface: None,
        url: Some("https://example.com/connect/abc".into()),
        summary: String::new(),
        request_state: json!({ "challenge_id": "abc" }),
    }
}

fn confirm_challenge() -> ToolInputRequired {
    ToolInputRequired {
        producer: "tool:jira/approve".into(),
        key: "approve".into(),
        kind: "confirm".into(),
        message: Some("Deploy to prod?".into()),
        surface: None,
        url: None,
        summary: String::new(),
        request_state: Value::Null,
    }
}

fn a2ui_challenge() -> ToolInputRequired {
    ToolInputRequired {
        producer: "tool:jira/form".into(),
        key: "form".into(),
        kind: "a2ui".into(),
        message: None,
        surface: Some(json!({ "version": "1.0", "createSurface": {} })),
        url: None,
        summary: String::new(),
        request_state: Value::Null,
    }
}

fn operator_context(challenge: ToolInputRequired) -> ToolEvalContext {
    let mut ctx = test_context(vec![Arc::new(ChallengeProvider { challenge })]);
    ctx.allowed_tool_names.insert("jira_connect".into());
    ctx
}

#[tokio::test]
async fn operator_url_challenge_returns_link_message_and_request_state() {
    let ctx = operator_context(url_challenge());
    let result = eval_operator_tool_call(ctx, test_call("jira_connect"), &create_abort_signal())
        .await
        .unwrap();
    assert_eq!(
        result.output["url"], "https://example.com/connect/abc",
        "the operator result carries the link: {}",
        result.output
    );
    assert_eq!(result.output["message"], "Connect Jira");
    assert_eq!(result.output["request_state"]["challenge_id"], "abc");
    assert!(result.output.get("is_error").is_none());
}

#[tokio::test]
async fn operator_url_challenge_with_an_invalid_url_degrades_to_an_error() {
    // A non-https, non-loopback link fails the shared decision 6 policy, so
    // the operator exception doesn't apply: the call errors like any other
    // interaction instead of handing a plain-http link to the CLI.
    let mut challenge = url_challenge();
    challenge.url = Some("http://example.com/connect/abc".into());
    let ctx = operator_context(challenge);
    let result = eval_operator_tool_call(ctx, test_call("jira_connect"), &create_abort_signal())
        .await
        .unwrap();
    assert_eq!(result.output["is_error"], true, "{}", result.output);
    assert!(
        result.output["error"]
            .as_str()
            .unwrap()
            .contains("interactions are not supported here"),
        "{}",
        result.output
    );
}

#[tokio::test]
async fn operator_confirm_and_a2ui_challenges_still_fail_as_today() {
    for challenge in [confirm_challenge(), a2ui_challenge()] {
        let ctx = operator_context(challenge);
        let result =
            eval_operator_tool_call(ctx, test_call("jira_connect"), &create_abort_signal())
                .await
                .unwrap();
        assert_eq!(result.output["is_error"], true, "{}", result.output);
        assert!(
            result.output["error"]
                .as_str()
                .unwrap()
                .contains("interactions are not supported here"),
            "{}",
            result.output
        );
    }
}

#[tokio::test]
async fn model_origin_url_challenge_still_requires_the_durable_layer() {
    // The exception is operator-only: a model-origin round without the
    // durable interaction layer still degrades the challenge to an error.
    let ctx = operator_context(url_challenge());
    let results = eval_tool_calls(
        &ctx,
        vec![test_call("jira_connect")],
        &create_abort_signal(),
    )
    .await
    .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].output["is_error"], true, "{}", results[0].output);
}
