//! Exercise real Claude subprocesses through allowlisted middleware tools.
use super::{
    documentation::fenced_blocks,
    fixture::{config, Fixture, Probe},
};
use harnx_claude_hook_tools::ClaudeHookToolset;
use harnx_core::{
    abort::{create_abort_signal, AbortSignal},
    config_data::ConfigData,
    tool::{ToolCall, ToolError, ToolProvider, ToolProviderOutput, ToolResult},
};
use harnx_engine::tool::{eval_tool_calls, eval_tool_round, ToolRoundOutcome};
use harnx_runtime::middleware::{with_resume, AcceptedInteraction, MiddlewareResume};
use harnx_toolset::Toolset;
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

const GUIDE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/middleware-claude-hooks.md"
));
const HOOK: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../harnx-claude-hook-tools/tests/fixtures/hook.py"
);

fn command(mode: &str) -> Value {
    let python = which::which("python3")
        .or_else(|_| which::which("python"))
        .expect("Python installed");
    json!([python, HOOK, mode])
}

fn documented_entries(guide: &str) -> Vec<Value> {
    let blocks = fenced_blocks(guide);
    assert_eq!(
        blocks.iter().map(|b| b.0.as_str()).collect::<Vec<_>>(),
        ["yaml", "yaml"]
    );
    let server: Value = serde_yaml::from_str(&blocks[0].1).unwrap();
    assert_eq!(server["command"], "harnx-claude-hook-tools");
    let config: ConfigData = serde_yaml::from_str(&blocks[1].1).unwrap();
    serde_json::to_value(config.middleware)
        .unwrap()
        .as_array()
        .unwrap()
        .clone()
}

struct CompatProvider {
    toolset: ClaudeHookToolset,
    probe: Arc<Probe>,
    command: Value,
    calls: Mutex<Vec<Value>>,
    replies: Mutex<Vec<Value>>,
}

#[async_trait::async_trait]
impl ToolProvider for CompatProvider {
    fn name(&self) -> &str {
        "claude-fixture"
    }
    fn has_tool(&self, name: &str) -> bool {
        matches!(name, "claude_hook_run" | "bash_exec")
    }
    async fn call_tool(
        &self,
        name: &str,
        mut args: Value,
        _abort: &AbortSignal,
    ) -> Result<ToolProviderOutput, ToolError> {
        if name == "bash_exec" {
            return self.probe.invoke(name, args).await;
        }
        self.calls.lock().push(args.clone());
        // Substitute only the external dcg executable. Documentation script,
        // tool schema, runner and hook protocol execute unchanged.
        if args["command"] == json!(["dcg"]) {
            args["command"] = self.command.clone();
        }
        let reply = self
            .toolset
            .invoke("run", args, CancellationToken::new())
            .await
            .map_err(|error| ToolError::Recoverable(error.into()))?;
        self.replies.lock().push(reply.clone());
        Ok(ToolProviderOutput::new(reply))
    }
}

async fn fixture(entries: Vec<Value>, mode: &str) -> (Fixture, Arc<CompatProvider>) {
    let config = config(entries);
    let spec = ClaudeHookToolset::default().tools().remove(0);
    config.write().nats_tool_declarations.write().push(
        serde_json::from_value(json!({
            "name":"claude_hook_run", "description":spec.description, "parameters":spec.input_schema
        }))
        .unwrap(),
    );
    let mut fixture = Fixture::with_config(config).await;
    let provider = Arc::new(CompatProvider {
        toolset: ClaudeHookToolset::default(),
        probe: fixture.probe.clone(),
        command: command(mode),
        calls: Mutex::new(vec![]),
        replies: Mutex::new(vec![]),
    });
    fixture.ctx.providers.push(provider.clone());
    fixture.ctx.allowed_tool_names.insert("bash_exec".into());
    (fixture, provider)
}

async fn eval(fixture: &Fixture, args: Value) -> ToolResult {
    eval_tool_calls(
        &fixture.ctx,
        vec![ToolCall::new(
            "bash_exec".into(),
            args,
            Some("outer".into()),
            None,
        )],
        &create_abort_signal(),
    )
    .await
    .unwrap()
    .remove(0)
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_claude_documentation_denies_without_dispatch() {
    for mode in ["deny", "exit2"] {
        let (fixture, provider) = fixture(documented_entries(GUIDE), mode).await;
        let result = Box::pin(eval(&fixture, json!({"command":"original"}))).await;
        assert_eq!(result.output["is_error"], true);
        assert!(result.output["error"].as_str().unwrap().contains("blocked"));
        assert!(fixture.calls().is_empty());
        assert_eq!(provider.calls.lock().len(), 1);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_claude_documentation_mutates_before_and_after_next_and_handles_crlf() {
    let guide = super::documentation::normalize_line_endings(GUIDE);
    for guide in [guide.clone(), guide.replace('\n', "\r\n")] {
        let (fixture, provider) = fixture(documented_entries(&guide), "mutate").await;
        let result = Box::pin(eval(
            &fixture,
            json!({"command":"original", "env":{"KEEP":"yes"}}),
        ))
        .await;
        assert_eq!(fixture.calls().len(), 1);
        assert_eq!(fixture.calls()[0].1["command"], "rewritten");
        assert_eq!(result.output["result"]["post"]["command"], "rewritten");
        assert_eq!(result.output["result"]["post"]["env"]["KEEP"], "yes");
        assert_eq!(
            result.output["claude_hook_context"],
            json!(["pre context", "post context"])
        );
        let calls = provider.calls.lock();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["event"], "PreToolUse");
        assert_eq!(calls[1]["event"], "PostToolUse");
        assert_eq!(calls[1]["payload"]["tool_input"]["command"], "rewritten");
        assert_eq!(calls[1]["payload"]["tool_response"]["command"], "rewritten");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_claude_documentation_errors_fail_closed_and_post_failures_are_observed() {
    let (fixture, _) = fixture(documented_entries(GUIDE), "exit1").await;
    let result = Box::pin(eval(&fixture, json!({"command":"original"}))).await;
    assert_eq!(result.output["is_error"], true);
    assert!(fixture.calls().is_empty());
    let (fixture, provider) = self::fixture(documented_entries(GUIDE), "mutate").await;
    let result = Box::pin(eval(
        &fixture,
        json!({"command":"original", "recoverable":true}),
    ))
    .await;
    assert_eq!(result.output["is_error"], true);
    assert_eq!(provider.calls.lock()[1]["event"], "PostToolUseFailure");
    assert!(provider.calls.lock()[1]["payload"]["error"]
        .as_str()
        .unwrap()
        .contains("recoverable probe failure"));
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_claude_persistent_jsonl_mutates_denies_and_bypasses_chain() {
    let args = json!({"command":command("--persistent"), "persistent":true, "event":"PreToolUse", "payload":{}});
    // Build the request from current arguments without relying on a host shell.
    let source = format!(
        r#"
request=$(tool_input | jq --argjson request '{args}' '. as $info | $request | .payload = {{session_id:"session",tool_name:$info.tool,tool_input:$info.arguments}}')
r=$(claude_hook_run --json "$request")
if [ "$(echo "$r" | jq -r '.decision')" = deny ]; then
  deny "$(echo "$r" | jq -r '.reason')"
else
  next "$(echo "$r" | jq -c '.tool_input')"
fi
"#
    );
    let entries =
        vec![json!({"name":"compat", "match":".*", "bash":source, "tools":["claude_hook_run"]})];
    let (fixture, provider) = fixture(entries, "mutate").await;
    for _ in 0..2 {
        let result = Box::pin(eval(&fixture, json!({"command":"original"}))).await;
        assert_eq!(result.output["command"], "rewritten");
    }
    let result = Box::pin(eval(
        &fixture,
        json!({"command":"original", "action":"deny"}),
    ))
    .await;
    assert_eq!(result.output["is_error"], true);
    assert_eq!(fixture.calls().len(), 2);
    assert_eq!(provider.calls.lock().len(), 3);
    let replies = provider.replies.lock();
    assert_eq!(replies[0]["result"]["diagnostics"]["count"], 1);
    assert_eq!(replies[1]["result"]["diagnostics"]["count"], 2);
}

async fn round(fixture: &Fixture) -> ToolRoundOutcome {
    eval_tool_round(
        &fixture.ctx,
        vec![ToolCall::new(
            "bash_exec".into(),
            json!({"command":"original"}),
            Some("outer".into()),
            None,
        )],
        vec![],
        &create_abort_signal(),
    )
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_claude_documentation_ask_resumes_without_repeating_hooks_or_next() {
    for mode in ["ask-pre", "ask-post"] {
        for action in ["approve", "deny"] {
            let (mut fixture, provider) = fixture(documented_entries(GUIDE), mode).await;
            fixture.ctx.on_interaction = Some(Arc::new(|_| Box::pin(async { Ok(()) })));
            let ToolRoundOutcome::InputRequired(mut suspension) = Box::pin(round(&fixture)).await
            else {
                panic!("expected Claude ask to suspend");
            };
            assert_eq!(suspension.pending.len(), 1);
            let pending = suspension.pending.remove(0);
            assert_eq!(
                pending.interaction.key,
                if mode == "ask-pre" {
                    "claude-pre"
                } else {
                    "claude-post"
                }
            );
            assert_eq!(fixture.calls().len(), usize::from(mode == "ask-post"));
            let resume = MiddlewareResume {
                tool_input: None,
                checkpoint: pending.checkpoint,
                responses: vec![AcceptedInteraction {
                    request: pending.interaction,
                    response: harnx_core::interaction::InteractionResponse {
                        action: action.into(),
                        data: Default::default(),
                        note: None,
                    },
                }],
            };
            let ToolRoundOutcome::Completed(results) =
                with_resume(resume, Box::pin(round(&fixture))).await
            else {
                panic!("accepted answer must complete call");
            };
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].output["is_error"] == true, action == "deny");
            let ran = mode == "ask-post" || action == "approve";
            assert_eq!(fixture.calls().len(), usize::from(ran));
            assert_eq!(provider.calls.lock().len(), if ran { 2 } else { 1 });
        }
    }
}
