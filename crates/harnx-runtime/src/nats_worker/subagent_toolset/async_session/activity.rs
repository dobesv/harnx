//! The activity sample a wait returns while the child is still running.
//!
//! It answers "is the child making progress, and roughly on what?" in a
//! couple of kilobytes: the live counters the progress watcher already keeps,
//! and the child's latest tool step trimmed hard. It is not a transcript and
//! carries no reasoning; `session_load` reads the turn when the parent wants
//! detail.

use harnx_core::api_types::CompletionTokenUsage;
use harnx_core::event::SubAgentProgress;
use harnx_core::tool::ToolCall;
use serde::Serialize;

/// Narration kept from the latest step, in bytes. The opening sentence says
/// what the child is about to do; the rest is detail the parent can load.
const NARRATION_MAX_BYTES: usize = 300;
/// Calls listed from the latest step; any more are counted, not listed.
const CALLS_MAX: usize = 4;
/// Compact JSON arguments kept per call, in bytes.
const ARGUMENTS_MAX_BYTES: usize = 160;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Activity {
    /// Tool calls the child has made, as far as this process's watcher saw.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool_call_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) usage: Option<CompletionTokenUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) latest: Option<LatestStep>,
}

impl Activity {
    /// The sample from the watcher's live snapshot and the latest durable
    /// step; `None` when neither has anything to say.
    pub(crate) fn new(live: Option<&SubAgentProgress>, latest: Option<LatestStep>) -> Option<Self> {
        if live.is_none() && latest.is_none() {
            return None;
        }
        Some(Self {
            tool_call_count: live.map(|live| live.tool_call_count),
            usage: live.map(|live| live.usage.clone()),
            title: live.and_then(|live| live.title.clone()),
            latest,
        })
    }
}

/// The child's most recent durable tool step.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct LatestStep {
    /// Durable log index of the step, for `session_load` reads around it.
    pub(crate) index: u64,
    /// The opening of the assistant's narration for the step.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub(crate) text: String,
    /// `name {arguments}` per call, arguments trimmed.
    pub(crate) calls: Vec<String>,
}

impl LatestStep {
    /// Thinking is left out on purpose: delegation results never show the
    /// parent the child's reasoning.
    pub(crate) fn new(index: u64, text: &str, calls: &[ToolCall]) -> Self {
        let mut listed: Vec<String> = calls.iter().take(CALLS_MAX).map(describe_call).collect();
        if calls.len() > CALLS_MAX {
            listed.push(format!("… {} more", calls.len() - CALLS_MAX));
        }
        Self {
            index,
            text: trimmed(text.trim(), NARRATION_MAX_BYTES),
            calls: listed,
        }
    }
}

fn describe_call(call: &ToolCall) -> String {
    let arguments = match &call.arguments {
        serde_json::Value::Object(map) if map.is_empty() => return call.name.clone(),
        serde_json::Value::Null => return call.name.clone(),
        arguments => arguments.to_string(),
    };
    format!("{} {}", call.name, trimmed(&arguments, ARGUMENTS_MAX_BYTES))
}

/// `text` cut to at most `max` bytes on a character boundary, with an
/// ellipsis when anything was cut.
fn trimmed(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    format!("{}…", &text[..text.floor_char_boundary(max)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: None,
            name: name.into(),
            arguments,
            thought_signature: None,
            reasoning_provenance: None,
        }
    }

    #[test]
    fn latest_step_trims_narration_and_arguments_on_char_boundaries() {
        let narration = "✓".repeat(200);
        let step = LatestStep::new(
            7,
            &format!("  {narration}  "),
            &[call("fs_read", json!({"path": "é".repeat(200)}))],
        );
        assert_eq!(step.index, 7);
        assert!(step.text.ends_with('…'));
        assert!(step.text.len() <= NARRATION_MAX_BYTES + '…'.len_utf8());
        assert!(narration.starts_with(step.text.trim_end_matches('…')));
        let [described] = step.calls.as_slice() else {
            panic!("one call: {:?}", step.calls);
        };
        assert!(
            described.starts_with("fs_read {\"path\":\"é"),
            "{described}"
        );
        assert!(described.len() <= "fs_read ".len() + ARGUMENTS_MAX_BYTES + '…'.len_utf8());
    }

    #[test]
    fn latest_step_lists_a_few_calls_and_counts_the_rest() {
        let calls: Vec<_> = (0..6)
            .map(|n| call(&format!("tool_{n}"), json!({})))
            .collect();
        let step = LatestStep::new(1, "", &calls);
        assert_eq!(
            step.calls,
            ["tool_0", "tool_1", "tool_2", "tool_3", "… 2 more"]
        );
        let value = serde_json::to_value(&step).unwrap();
        assert!(value.get("text").is_none(), "empty narration is omitted");
    }

    #[test]
    fn activity_needs_a_live_snapshot_or_a_step() {
        assert!(Activity::new(None, None).is_none());
        let step = Activity::new(None, Some(LatestStep::new(1, "next", &[]))).unwrap();
        let value = serde_json::to_value(&step).unwrap();
        assert_eq!(
            value,
            json!({"latest": {"index": 1, "text": "next", "calls": []}})
        );
    }
}
