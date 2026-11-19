//! The stub model: publishes the test surface on its first round, then gates
//! or steers per mode so mid-turn effects can be observed round by round.

use harnx_core::tool::ToolCall;
use harnx_runtime::agent_loop::AgentCallFn;
use harnx_runtime::client::CompletionTokenUsage;
use parking_lot::Mutex;
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub(super) static MODEL_ROUNDS: Mutex<Vec<Vec<String>>> = Mutex::new(Vec::new());
/// Set once the test has submitted its gated mid-turn action; the stub model
/// keeps issuing tool calls until then, then runs one more round and finishes.
pub(super) static GATED_SUBMITTED: AtomicBool = AtomicBool::new(false);

pub(super) fn record_round(
    input: &harnx_runtime::config::input::Input,
    config: &harnx_runtime::config::GlobalConfig,
) {
    let texts = harnx_runtime::config::input::build_messages(input, config)
        .expect("build model messages")
        .iter()
        .map(|message| serde_json::to_string(message).expect("serialize model message"))
        .collect();
    MODEL_ROUNDS.lock().push(texts);
}

/// Marker the note-visibility test injects as its action's user message; the
/// gated stub model waits until the round's input actually contains it before
/// finishing, because the note's queue push races with round preparation.
pub(super) const GATED_NOTE_MARKER: &str = "deployment reviewed by the operator";
/// Marker the late-reply `turn` test injects: the steered stub model keeps
/// the turn alive until its input carries this steering text.
pub(super) const STEER_TURN_MARKER: &str = "rolling out now";
/// Marker carried by the mid-turn `invisible` action's data: it must never
/// appear in any model-visible round.
pub(super) const INVISIBLE_MARKER: &str = "quiet-invisible-choice-42";

/// A stub model whose first round calls the publisher tool. Later behavior
/// depends on `mode`:
///
/// - "single": reply "all done" — no gating.
/// - "gated": keep calling the counter tool until `GATED_SUBMITTED` AND the
///   current round's input carries `GATED_NOTE_MARKER` (the note may reach
///   the queue after this round's input was already prepared), then finish.
/// - "steered": keep calling the counter tool until the current round's
///   input carries `STEER_TURN_MARKER` (a `turn`-visibility action joined
///   the running turn), then finish.
pub(super) fn surface_call_fn(mode: &'static str) -> AgentCallFn {
    Arc::new(move |input, config, _abort| {
        record_round(input, config);
        let first = MODEL_ROUNDS.lock().len() == 1;
        Box::pin(async move {
            if first {
                return Ok((
                    "calling the publisher".to_string(),
                    None,
                    vec![ToolCall::new(
                        "surfaces_publish".to_string(),
                        json!({}),
                        Some("call-publish".to_string()),
                        None,
                    )],
                    CompletionTokenUsage::default(),
                ));
            }
            if mode == "single" {
                return Ok((
                    "all done".to_string(),
                    None,
                    vec![],
                    CompletionTokenUsage::default(),
                ));
            }
            if mode == "steered" {
                let steered = MODEL_ROUNDS
                    .lock()
                    .last()
                    .is_some_and(|texts| texts.iter().any(|text| text.contains(STEER_TURN_MARKER)));
                if !steered {
                    return Ok((
                        "still working".to_string(),
                        None,
                        vec![ToolCall::new(
                            "surfaces_counter".to_string(),
                            json!({}),
                            Some("call-tick".to_string()),
                            None,
                        )],
                        CompletionTokenUsage::default(),
                    ));
                }
                return Ok((
                    "all done".to_string(),
                    None,
                    vec![],
                    CompletionTokenUsage::default(),
                ));
            }
            if !GATED_SUBMITTED.load(Ordering::SeqCst) {
                return Ok((
                    "still working".to_string(),
                    None,
                    vec![ToolCall::new(
                        "surfaces_counter".to_string(),
                        json!({}),
                        Some("call-tick".to_string()),
                        None,
                    )],
                    CompletionTokenUsage::default(),
                ));
            }
            // The submission completed before the flag was set, but this
            // round's input may still predate the note's queue push. Keep
            // the turn alive until the note is really in the model input.
            let note_seen = MODEL_ROUNDS
                .lock()
                .last()
                .is_some_and(|texts| texts.iter().any(|text| text.contains(GATED_NOTE_MARKER)));
            if !note_seen {
                return Ok((
                    "still working".to_string(),
                    None,
                    vec![ToolCall::new(
                        "surfaces_counter".to_string(),
                        json!({}),
                        Some("call-tick".to_string()),
                        None,
                    )],
                    CompletionTokenUsage::default(),
                ));
            }
            Ok((
                "all done".to_string(),
                None,
                vec![],
                CompletionTokenUsage::default(),
            ))
        })
    })
}

pub(super) fn model_round_texts() -> Vec<Vec<String>> {
    MODEL_ROUNDS.lock().clone()
}

pub(super) fn reset_model_state() {
    MODEL_ROUNDS.lock().clear();
    GATED_SUBMITTED.store(false, Ordering::SeqCst);
}
