//! Applies the tool-repetition guard to one tool round: decides which calls
//! run, notes results that repeat, and reports a stop.

use crate::config::{GlobalConfig, Input};
use crate::tool::{ToolCall, ToolEvalContext, ToolResult};
use harnx_core::event::{AgentEvent, NoticeEvent};
use harnx_core::loop_guard::{append_note, RepetitionTerminal, ToolRepeatGuard, ToolRepeatVerdict};
use parking_lot::Mutex;
use serde_json::json;
use std::sync::Arc;

/// The guard for one turn's tool loop.
pub type ToolLoopGuardHandle = Arc<Mutex<ToolRepeatGuard>>;

pub(crate) enum Screened {
    Proceed {
        to_eval: Vec<ToolCall>,
        refused: Vec<ToolResult>,
    },
    Stop(RepetitionTerminal),
}

/// Start a tool round on the turn's guard. Returns the guard to screen the
/// round with, or `None` when loop detection is off for the active agent.
pub(crate) fn begin_round<'a>(
    guard: Option<&'a Mutex<ToolRepeatGuard>>,
    config: &GlobalConfig,
    input: &Input,
) -> Option<&'a Mutex<ToolRepeatGuard>> {
    let enabled = config
        .read()
        .loop_detection
        .resolve(input.agent().loop_detection())
        .tool_calls;
    let guard = guard.filter(|_| enabled)?;
    let user_message_arrived = input.injected_user_text().is_some();
    let marker = compaction_marker(config);
    guard.lock().begin_round(user_message_arrived, marker);
    Some(guard)
}

/// Decide which of this round's calls run. Refused calls get their error
/// result here and are shown as blocked.
pub(crate) fn screen_round(
    guard: Option<&Mutex<ToolRepeatGuard>>,
    calls: &[ToolCall],
    eval: &ToolEvalContext,
) -> Screened {
    let Some(guard) = guard else {
        return Screened::Proceed {
            to_eval: calls.to_vec(),
            refused: Vec::new(),
        };
    };
    let mut guard = guard.lock();
    // A round's calls are one model response.
    guard.begin_batch();
    let now = chrono::Utc::now();
    let mut to_eval = Vec::new();
    let mut refused = Vec::new();
    // Screen the calls the engine would run. It keeps one call per id, so a
    // copy is not another request and must not count as one.
    for call in ToolCall::dedup(calls.to_vec()) {
        match guard.decide(&call, now) {
            ToolRepeatVerdict::Run => to_eval.push(call),
            ToolRepeatVerdict::Refuse(refusal) => {
                let output = json!({
                    "is_error": true,
                    "error": refusal.message(),
                    "loop_guard": "refused",
                });
                (eval.emit_tool_blocked_fn)(&call, &output);
                refused.push(ToolResult::new(call, output));
            }
            ToolRepeatVerdict::Stop(terminal) => return Screened::Stop(terminal),
        }
    }
    Screened::Proceed { to_eval, refused }
}

/// Record executed results, note the ones that repeat, and merge in the
/// refusals in the order the model requested the calls.
pub(crate) fn finish_round(
    guard: Option<&Mutex<ToolRepeatGuard>>,
    mut results: Vec<ToolResult>,
    refused: Vec<ToolResult>,
    calls: &[ToolCall],
) -> Vec<ToolResult> {
    if let Some(guard) = guard {
        note_repeats(&mut guard.lock(), &mut results);
    }
    if refused.is_empty() {
        return results;
    }
    results.extend(refused);
    order_by_calls(results, calls)
}

/// The result every call in a stopped round gets.
pub(crate) fn stopped_output(terminal: &RepetitionTerminal) -> serde_json::Value {
    json!({
        "is_error": true,
        "error": format!("harnx ended the turn: {}.", terminal.reason()),
        "loop_guard": "stopped",
    })
}

fn note_repeats(guard: &mut ToolRepeatGuard, results: &mut [ToolResult]) {
    let now = chrono::Utc::now();
    for result in results {
        let Some(note) = guard.record(&result.call, &result.output, now) else {
            continue;
        };
        let text = note.text();
        // The engine already emitted this result to the UI, so show the note
        // live as a notice; the persisted result carries it too.
        harnx_core::sink::emit_agent_event(AgentEvent::Notice(NoticeEvent::Warning(format!(
            "{}: {text}",
            result.call.name
        ))));
        append_note(&mut result.output, &text);
    }
}

// Mid-turn compaction moves history into `compressed_messages`, so its length
// changing means earlier results left the model's context.
fn compaction_marker(config: &GlobalConfig) -> usize {
    config
        .read()
        .session
        .as_ref()
        .map_or(0, |session| session.compressed_messages.len())
}

fn order_by_calls(mut results: Vec<ToolResult>, calls: &[ToolCall]) -> Vec<ToolResult> {
    results.sort_by_key(|result| {
        calls
            .iter()
            .position(|call| call.id == result.call.id)
            .unwrap_or(usize::MAX)
    });
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: &str) -> ToolCall {
        ToolCall::new(
            "fs_read".into(),
            json!({"path": id}),
            Some(id.to_string()),
            None,
        )
    }

    #[test]
    fn refused_results_are_merged_in_request_order() {
        let calls = vec![call("a"), call("b"), call("c")];
        let executed = vec![
            ToolResult::new(call("c"), json!("c")),
            ToolResult::new(call("a"), json!("a")),
        ];
        let refused = vec![ToolResult::new(call("b"), json!({"loop_guard": "refused"}))];
        let merged = finish_round(None, executed, refused, &calls);
        let ids: Vec<_> = merged.iter().map(|r| r.call.id.clone().unwrap()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
    }

    #[test]
    fn without_refusals_the_engine_order_is_kept() {
        let calls = vec![call("a"), call("b")];
        let executed = vec![
            ToolResult::new(call("b"), json!("b")),
            ToolResult::new(call("a"), json!("a")),
        ];
        let kept = finish_round(None, executed, vec![], &calls);
        let ids: Vec<_> = kept.iter().map(|r| r.call.id.clone().unwrap()).collect();
        assert_eq!(ids, ["b", "a"]);
    }

    async fn eval_context(config: &GlobalConfig) -> ToolEvalContext {
        let scope = harnx_core::instance::ServerScope::new();
        crate::tool::build_tool_eval_context(crate::tool::BuildToolEvalContextParams::new(
            config, &scope,
        ))
        .await
    }

    #[tokio::test]
    async fn over_limit_calls_in_one_round_are_refused_and_the_next_round_stops() {
        let eval = eval_context(&GlobalConfig::default()).await;
        let guard = Mutex::new(ToolRepeatGuard::default());
        let now = chrono::Utc::now();
        for _ in 0..4 {
            guard.lock().record(&call("a"), &json!("same"), now);
            guard.lock().record(&call("b"), &json!("same"), now);
        }

        // The model sent both calls before it had seen either refusal.
        let Screened::Proceed { to_eval, refused } =
            screen_round(Some(&guard), &[call("a"), call("b")], &eval)
        else {
            panic!("two refusals in one response must not end the turn");
        };
        assert!(to_eval.is_empty());
        assert_eq!(refused.len(), 2);

        // Asking again in the next round, after seeing them, ends the turn.
        assert!(matches!(
            screen_round(Some(&guard), &[call("b")], &eval),
            Screened::Stop(_)
        ));
    }

    #[tokio::test]
    async fn a_call_sent_twice_under_one_id_is_refused_once() {
        let eval = eval_context(&GlobalConfig::default()).await;
        let guard = Mutex::new(ToolRepeatGuard::default());
        let now = chrono::Utc::now();
        for _ in 0..4 {
            guard.lock().record(&call("a"), &json!("same"), now);
        }

        // The engine runs one call per id, so the copy is not a second request
        // and must not turn the refusal into a stop.
        let Screened::Proceed { to_eval, refused } =
            screen_round(Some(&guard), &[call("a"), call("a")], &eval)
        else {
            panic!("a duplicated call must be refused, not stopped");
        };
        assert!(to_eval.is_empty());
        assert_eq!(refused.len(), 1);
    }
}
