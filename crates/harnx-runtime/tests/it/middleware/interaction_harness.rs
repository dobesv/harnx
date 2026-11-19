//! Shared suspension/resume harness for the middleware interaction tests
//! (`confirm`, `interact`): real interpreters and engine outcomes, with
//! checkpoints kept outside the tool-call arguments.

use super::fixture::{call, Fixture};
use harnx_core::{abort::create_abort_signal, interaction::InteractionResponse, tool::ToolResult};
use harnx_engine::tool::{
    eval_tool_round, PendingToolInteraction, ToolCallCheckpoint, ToolRoundOutcome,
};
use harnx_runtime::middleware::{with_resume, AcceptedInteraction, MiddlewareResume};
use serde_json::Value;
use std::sync::Arc;

#[derive(Default)]
pub(super) struct MemoryStore {
    checkpoint: Option<ToolCallCheckpoint>,
    responses: Vec<AcceptedInteraction>,
}

pub(super) struct Harness {
    pub(super) fixture: Fixture,
    store: MemoryStore,
}

impl Harness {
    pub(super) async fn new(entries: Vec<Value>) -> Self {
        let mut fixture = Fixture::new(entries).await;
        fixture.ctx.on_interaction = Some(Arc::new(|_| Box::pin(async { Ok(()) })));
        Self {
            fixture,
            store: MemoryStore::default(),
        }
    }

    /// One middleware round, resuming from the stored checkpoint and
    /// remembering any new suspension it produces.
    pub(super) async fn attempt(&mut self, args: Value) -> ToolRoundOutcome {
        let abort = create_abort_signal();
        let future = eval_tool_round(&self.fixture.ctx, vec![call(args)], vec![], &abort);
        let outcome = match &self.store.checkpoint {
            Some(checkpoint) => {
                with_resume(
                    MiddlewareResume {
                        tool_input: None,
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

    /// Whether an attempt has suspended, for tests that expect none.
    pub(super) fn suspended(&self) -> bool {
        self.store.checkpoint.is_some()
    }

    /// The stored checkpoint, for tests that tamper with it.
    pub(super) fn checkpoint_mut(&mut self) -> Option<&mut ToolCallCheckpoint> {
        self.store.checkpoint.as_mut()
    }

    /// The last accepted request's payload, for tamper tests.
    pub(super) fn last_request_payload_mut(&mut self) -> &mut Value {
        let accepted = self
            .store
            .responses
            .last_mut()
            .expect("an answer must be accepted first");
        &mut accepted.request.payload
    }

    /// The last accepted response, for tests that tamper with it.
    pub(super) fn last_response_mut(&mut self) -> &mut InteractionResponse {
        let accepted = self
            .store
            .responses
            .last_mut()
            .expect("an answer must be accepted first");
        &mut accepted.response
    }

    /// Accepts a confirm-style answer: an action with no data payload.
    pub(super) fn accept(&mut self, pending: &PendingToolInteraction, action: &str) {
        self.accept_data(pending, action, Value::Null);
    }

    /// Accepts an interact-style answer: an action plus the user's input.
    pub(super) fn accept_data(
        &mut self,
        pending: &PendingToolInteraction,
        action: &str,
        data: Value,
    ) {
        self.store.responses.push(AcceptedInteraction {
            request: pending.interaction.clone(),
            response: InteractionResponse {
                action: action.into(),
                data: data.as_object().cloned().unwrap_or_default(),
                note: Some("human response".into()),
            },
        });
    }
}

pub(super) fn pending(outcome: ToolRoundOutcome) -> PendingToolInteraction {
    let ToolRoundOutcome::InputRequired(mut suspension) = outcome else {
        panic!("expected suspension: {outcome:?}")
    };
    suspension.pending.remove(0)
}

pub(super) fn completed(outcome: ToolRoundOutcome) -> ToolResult {
    let ToolRoundOutcome::Completed(mut results) = outcome else {
        panic!("expected completion: {outcome:?}")
    };
    assert_eq!(results.len(), 1);
    results.remove(0)
}
