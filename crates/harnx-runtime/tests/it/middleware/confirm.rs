//! Real interpreters and engine outcomes, with checkpoints kept outside arguments.
mod binding;
mod documentation;
mod stash;
mod traversal;

use super::fixture::{bash, call, python, Fixture};
use harnx_core::{abort::create_abort_signal, interaction::InteractionResponse, tool::ToolResult};
use harnx_engine::tool::{
    eval_tool_round, PendingToolInteraction, ToolCallCheckpoint, ToolRoundOutcome,
};
use harnx_runtime::middleware::{with_resume, AcceptedInteraction, MiddlewareResume};
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Default)]
struct MemoryStore {
    checkpoint: Option<ToolCallCheckpoint>,
    responses: Vec<AcceptedInteraction>,
}

struct Harness {
    fixture: Fixture,
    store: MemoryStore,
}

impl Harness {
    async fn new(entries: Vec<Value>) -> Self {
        let mut fixture = Fixture::new(entries).await;
        fixture.ctx.on_interaction = Some(Arc::new(|_| Box::pin(async { Ok(()) })));
        Self {
            fixture,
            store: MemoryStore::default(),
        }
    }

    async fn attempt(&mut self, args: Value) -> ToolRoundOutcome {
        let abort = create_abort_signal();
        let future = eval_tool_round(&self.fixture.ctx, vec![call(args)], vec![], &abort);
        let outcome = match &self.store.checkpoint {
            Some(checkpoint) => {
                with_resume(
                    MiddlewareResume {
                        checkpoint: checkpoint.clone(),
                        responses: self.store.responses.clone(),
                    },
                    future,
                )
                .await
            }
            None => future.await,
        }
        .unwrap();
        if let ToolRoundOutcome::InputRequired(suspension) = &outcome {
            assert!(suspension.completed.is_empty());
            assert_eq!(suspension.pending.len(), 1);
            self.store.checkpoint = Some(suspension.pending[0].checkpoint.clone());
        }
        outcome
    }

    fn accept(&mut self, pending: &PendingToolInteraction, action: &str) {
        self.store.responses.push(AcceptedInteraction {
            request: pending.interaction.clone(),
            response: InteractionResponse {
                action: action.into(),
                data: Default::default(),
                note: Some("human response".into()),
            },
        });
    }
}

fn pending(outcome: ToolRoundOutcome) -> PendingToolInteraction {
    let ToolRoundOutcome::InputRequired(mut suspension) = outcome else {
        panic!("expected suspension: {outcome:?}")
    };
    suspension.pending.remove(0)
}

fn completed(outcome: ToolRoundOutcome) -> ToolResult {
    let ToolRoundOutcome::Completed(mut results) = outcome else {
        panic!("expected completion: {outcome:?}")
    };
    assert_eq!(results.len(), 1);
    results.remove(0)
}

async fn decision(entry: Value, action: &str) {
    let mut harness = Harness::new(vec![entry]).await;
    let request = pending(harness.attempt(json!({"value":"original"})).await);
    assert_eq!(request.interaction.key, "gate");
    assert_eq!(request.interaction.kind, "confirm");
    assert_eq!(request.interaction.payload["harnx"]["kind"], "confirm");
    assert_eq!(request.checkpoint.state["version"], 1);
    assert!(harness.fixture.calls().is_empty());
    harness.accept(&request, action);
    let result = completed(harness.attempt(json!({"value":"original"})).await);
    if action == "approve" {
        assert_eq!(result.output["value"], "original");
        assert_eq!(harness.fixture.calls().len(), 1);
    } else {
        assert_eq!(result.output["error"], "user denied");
        assert_eq!(result.output["is_error"], true);
        assert!(harness.fixture.calls().is_empty());
    }
}

const BASH_DECISION: &str =
    "if confirm --id gate 'Proceed?'; then next; else deny 'user denied'; fi";
const PYTHON_DECISION: &str =
    "if confirm('Proceed?', id='gate'):\n    call_next()\nelse:\n    deny('user denied')";

#[tokio::test(flavor = "multi_thread")]
async fn middleware_confirm_bash_approve() {
    decision(bash(BASH_DECISION), "approve").await;
}
#[tokio::test(flavor = "multi_thread")]
async fn middleware_confirm_bash_deny() {
    decision(bash(BASH_DECISION), "deny").await;
}
#[tokio::test(flavor = "multi_thread")]
async fn middleware_confirm_python_approve() {
    decision(python(PYTHON_DECISION), "approve").await;
}
#[tokio::test(flavor = "multi_thread")]
async fn middleware_confirm_python_deny() {
    decision(python(PYTHON_DECISION), "deny").await;
}

async fn sequential(entry: Value) {
    let mut harness = Harness::new(vec![entry]).await;
    let first = pending(harness.attempt(json!({})).await);
    assert_eq!(first.interaction.key, "1");
    harness.accept(&first, "approve");
    let second = pending(harness.attempt(json!({})).await);
    assert_eq!(second.interaction.key, "2");
    assert_ne!(
        first.interaction.interaction_id,
        second.interaction.interaction_id
    );
    harness.accept(&second, "approve");
    let result = completed(harness.attempt(json!({})).await);
    assert_eq!(result.output, json!({"done":true}));
    assert!(harness.fixture.calls().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_confirm_bash_two_suspensions() {
    sequential(bash("confirm 'First?'; confirm 'Second?'; respond '{\"output\":{\"done\":true},\"is_error\":false}'")).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn middleware_confirm_python_two_suspensions() {
    sequential(python("confirm('First?')\nconfirm('Second?')\nrespond({'output': {'done': True}, 'is_error': False})")).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_confirm_inner_bash_tool_fails_even_on_error_allow() {
    let mut entry = bash("confirm 'Inside script?'; next");
    entry["on_error"] = json!("allow");
    let harness = Harness::new(vec![entry]).await;
    let results = harnx_engine::tool::eval_tool_calls(
        &harness.fixture.ctx,
        vec![harnx_core::tool::ToolCall::new(
            "bash".into(),
            json!({"script":"probe_run --json '{}'"}),
            Some("bash-root".into()),
            None,
        )],
        &create_abort_signal(),
    )
    .await
    .unwrap();
    assert!(results[0]
        .output
        .to_string()
        .contains("interaction required, can't be answered inside a script"));
    assert!(harness.fixture.calls().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_confirm_operator_auto_approves_both_languages() {
    for entry in [bash(BASH_DECISION), python(PYTHON_DECISION)] {
        let fixture = Fixture::new(vec![entry]).await;
        let result = harnx_engine::tool::eval_operator_tool_call(
            fixture.ctx.clone(),
            call(json!({"value":"operator"})),
            &create_abort_signal(),
        )
        .await
        .unwrap();
        assert_eq!(result.output["value"], "operator");
        assert_eq!(fixture.calls().len(), 1);
    }
}
