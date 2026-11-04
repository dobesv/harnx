//! Project inner-call lifecycle onto the outer bash progress row.

use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
};

use harnx_core::tool::{ToolCall, ToolProgress, ToolUpdatePatch};
use serde_json::Value;

use super::{InnerCallObserver, InnerTraceEntry};

/// Running tool names shown in the aggregated progress title before the
/// remainder is collapsed to `+N`.
pub const AGGREGATOR_MAX_RUNNING_NAMES: usize = 3;

/// Trace lines kept in the aggregated progress markdown (the newest ones).
pub const AGGREGATOR_MAX_TRACE_LINES: usize = 10;

/// Title cap, mirroring `harnx_toolset::progress::TOOL_PROGRESS_MAX_STRING_BYTES`
/// (that module is private, so the value is repeated here).
pub(super) const AGGREGATOR_MAX_TITLE_BYTES: usize = 4 * 1024;

/// Markdown cap, mirroring `TOOL_PROGRESS_MAX_MARKDOWN_BYTES`.
pub(super) const AGGREGATOR_MAX_MARKDOWN_BYTES: usize = 64 * 1024;

/// Aggregates inner-call lifecycle into the outer `bash` tool's progress.
///
/// Title: `bash: <running tool names, max 3> (<done> done, <running> running)`.
/// Markdown: the newest [`AGGREGATOR_MAX_TRACE_LINES`] trace lines. Each event
/// emits one `ToolUpdatePatch` through the outer progress handle the
/// BashProvider received in `call_tool_with_progress`; the engine's
/// `RuntimeToolProgress` already coalesces those, so this adds no timer.
///
/// Child progress patch bodies (a child's own title/markdown) are NOT
/// surfaced: the title already names running tools and nested child markdown
/// would bloat the outer row. Only lifecycle counts and trace lines show.
pub struct InnerProgressAggregator {
    progress: Arc<dyn ToolProgress>,
    state: Mutex<AggregatorState>,
}

#[derive(Default)]
struct AggregatorState {
    /// Running calls in start order: admission sequence → tool name.
    running: BTreeMap<u64, String>,
    /// call_id → admission sequence, to remove finished calls in O(1).
    call_seqs: HashMap<String, u64>,
    next_seq: u64,
    done: usize,
    /// Newest trace lines last; capped at [`AGGREGATOR_MAX_TRACE_LINES`].
    lines: Vec<String>,
}

impl InnerProgressAggregator {
    /// Wrap the outer `bash` tool's progress handle.
    pub fn new(progress: Arc<dyn ToolProgress>) -> Self {
        Self {
            progress,
            state: Mutex::new(AggregatorState::default()),
        }
    }

    fn update_running(&self, mutate: impl FnOnce(&mut AggregatorState)) {
        let patch = {
            let mut state = self.state.lock().expect("aggregator state lock");
            mutate(&mut state);
            aggregated_patch(&state)
        };
        self.progress.update(patch);
    }
}

impl InnerCallObserver for InnerProgressAggregator {
    fn started(&self, call: &ToolCall, _args: &Value) {
        // The engine assigns IDs before admission, so the Started emitter
        // always sees one.
        let call_id = call.id.clone().expect("admitted inner call has an ID");
        let name = call.name.clone();
        self.update_running(|state| {
            let seq = state.next_seq;
            state.next_seq += 1;
            state.running.insert(seq, name);
            state.call_seqs.insert(call_id, seq);
        });
    }

    fn finished(&self, entry: &InnerTraceEntry) {
        let line = entry.to_progress_line();
        self.update_running(|state| {
            // Pre-admission rejections never started; they count as done
            // without ever having been running.
            if let Some(seq) = state.call_seqs.remove(&entry.call_id) {
                state.running.remove(&seq);
            }
            state.done += 1;
            state.lines.push(line);
            let overflow = state.lines.len().saturating_sub(AGGREGATOR_MAX_TRACE_LINES);
            state.lines.drain(0..overflow);
        });
    }
}

/// Snapshot the aggregate state as one patch. Bounded defensively: the
/// trace summary stays far below the toolset limits, but a pathological tool
/// name or args summary must not push the outer row over them.
fn aggregated_patch(state: &AggregatorState) -> ToolUpdatePatch {
    ToolUpdatePatch {
        title: Some(truncate_bytes(
            aggregated_title(state),
            AGGREGATOR_MAX_TITLE_BYTES,
        )),
        // Omit markdown while no call has finished yet, so the patch doesn't
        // clear the outer row's initial content.
        markdown: (!state.lines.is_empty())
            .then(|| truncate_bytes(state.lines.join("\n"), AGGREGATOR_MAX_MARKDOWN_BYTES)),
        ..ToolUpdatePatch::default()
    }
}

fn aggregated_title(state: &AggregatorState) -> String {
    let mut title = String::from("bash:");
    let names: Vec<&str> = state.running.values().map(String::as_str).collect();
    if !names.is_empty() {
        title.push(' ');
        title.push_str(
            &names
                .iter()
                .take(AGGREGATOR_MAX_RUNNING_NAMES)
                .copied()
                .collect::<Vec<_>>()
                .join(", "),
        );
        if names.len() > AGGREGATOR_MAX_RUNNING_NAMES {
            title.push_str(&format!(" +{}", names.len() - AGGREGATOR_MAX_RUNNING_NAMES));
        }
    }
    title.push_str(&format!(
        " ({} done, {} running)",
        state.done,
        state.running.len()
    ));
    title
}

/// Truncate at a UTF-8 character boundary so the byte cap can't split a
/// multi-byte character.
fn truncate_bytes(mut text: String, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text
}

#[cfg(test)]
mod tests {
    use super::truncate_bytes;

    #[test]
    fn truncate_bytes_respects_char_boundaries() {
        let text = "é".repeat(10);
        assert_eq!(text.len(), 20);
        let truncated = truncate_bytes(text.clone(), 5);
        assert_eq!(truncated, "éé".to_string());
        assert!(truncated.len() <= 5);
        assert_eq!(truncate_bytes(text.clone(), 20), text);
    }
}
