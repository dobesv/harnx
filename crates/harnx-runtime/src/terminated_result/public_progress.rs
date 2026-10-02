use super::{append_tail, lock_or_recover, INVOCATION_TEXT_TAIL_CAP_BYTES};
use harnx_core::{
    message::{MessageContent, MessageContentPart, MessageRole},
    session::SessionLogEntry,
};
use serde::Serialize;
use std::sync::Mutex;

pub const PUBLIC_REFERENCE_CAP: usize = 16;
const REFERENCE_BYTES_CAP: usize = 1024;

/// Available public output, not a claim that work finished or side effects stopped.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct PublicProgress {
    pub available: bool,
    pub output_excerpt: Option<String>,
    pub references: Vec<String>,
}

impl PublicProgress {
    pub fn observe_text(&mut self, text: &str) {
        let buffer = Mutex::new(self.output_excerpt.take().unwrap_or_default());
        append_tail(&buffer, text);
        self.output_excerpt =
            Some(lock_or_recover(&buffer).clone()).filter(|s| !s.trim().is_empty());
        self.available = self.output_excerpt.is_some() || !self.references.is_empty();
        self.observe_references(text);
        if let Some(output) = self.output_excerpt.clone() {
            self.observe_references(&output);
        }
    }

    fn observe_references(&mut self, text: &str) {
        for word in text.split(|c: char| c.is_whitespace() || "\"'`<>()[]{} ,".contains(c)) {
            if word.starts_with("cid:")
                && word.len() <= REFERENCE_BYTES_CAP
                && harnx_core::cid_url::CidUrl::parse(word).is_ok()
                && self.references.len() < PUBLIC_REFERENCE_CAP
                && !self.references.iter().any(|r| r == word)
            {
                self.references.push(word.to_owned());
            }
        }
        self.available = self.output_excerpt.is_some() || !self.references.is_empty();
    }

    pub fn observe_value(&mut self, value: &serde_json::Value) {
        // Bound traversal as well as retained output for large/nested tool payloads.
        fn visit(
            progress: &mut PublicProgress,
            value: &serde_json::Value,
            remaining: &mut usize,
            depth: usize,
        ) {
            if *remaining == 0 || depth > 16 {
                return;
            }
            *remaining -= 1;
            match value {
                serde_json::Value::String(text) => progress.observe_text(text),
                serde_json::Value::Array(items) => {
                    for item in items {
                        visit(progress, item, remaining, depth + 1);
                    }
                }
                serde_json::Value::Object(items) => {
                    for item in items.values() {
                        visit(progress, item, remaining, depth + 1);
                    }
                }
                _ => {}
            }
        }
        visit(self, value, &mut 256, 0);
    }

    pub fn from_entries(entries: &[(u64, SessionLogEntry)], prompt_seq: u64) -> Self {
        let end =
            crate::nats_session::invocation_terminal_seq(entries, prompt_seq).unwrap_or(u64::MAX);
        let mut progress = Self::default();
        for (_, entry) in entries
            .iter()
            .filter(|(seq, _)| *seq > prompt_seq && *seq <= end)
        {
            match entry {
                SessionLogEntry::Message {
                    role: MessageRole::Assistant,
                    content,
                    ..
                } => match content {
                    MessageContent::ToolCalls(calls) => progress.observe_text(&calls.text),
                    MessageContent::Text(text) => progress.observe_text(text),
                    MessageContent::Array(parts) => {
                        for part in parts {
                            if let MessageContentPart::Text { text } = part {
                                progress.observe_text(text);
                            }
                        }
                    }
                },
                SessionLogEntry::ToolCalls { text, .. } => progress.observe_text(text),
                SessionLogEntry::ToolResults { results, .. } => {
                    for result in results {
                        progress.observe_value(&result.output);
                    }
                }
                _ => {}
            }
        }
        progress
    }

    pub(super) fn bounded(&self) -> Self {
        let mut progress = Self::default();
        if let Some(output) = &self.output_excerpt {
            progress.observe_text(output);
        }
        for reference in &self.references {
            progress.observe_references(reference);
        }
        debug_assert!(progress
            .output_excerpt
            .as_ref()
            .is_none_or(|s| s.len() <= INVOCATION_TEXT_TAIL_CAP_BYTES));
        progress
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_progress_is_bounded_and_does_not_require_thinking() {
        let mut progress = PublicProgress::default();
        assert!(!progress.available);
        for i in 0..100 {
            progress.observe_text(&format!("result cid:media:test/session/{i:064x} "));
        }
        progress.observe_text(&"é".repeat(INVOCATION_TEXT_TAIL_CAP_BYTES));
        assert!(progress.available);
        assert_eq!(progress.references.len(), PUBLIC_REFERENCE_CAP);
        assert!(progress.output_excerpt.unwrap().len() <= INVOCATION_TEXT_TAIL_CAP_BYTES);
    }
}

#[cfg(test)]
mod durable_tests {
    use super::*;
    use crate::{synthesize_terminated_result, TerminationInputs, TerminationKind};

    #[test]
    fn nonstreaming_timeout_keeps_durable_artifact_references_but_not_later_turn_or_thinking() {
        let reference = format!("cid:media:test/session/{}", "a".repeat(64));
        let results = |text: String| SessionLogEntry::ToolResults {
            results: vec![harnx_core::session::ToolOutput {
                id: Some("call".into()),
                name: "artifact".into(),
                output: serde_json::json!({"uri": text}),
                markdown: None,
                content: vec![],
                switch_agent: None,
            }],
            timestamp: None,
        };
        let entries = vec![
            (
                2,
                SessionLogEntry::ToolCalls {
                    text: "Public checkpoint".into(),
                    thought: Some("private reasoning".into()),
                    calls: vec![],
                    timestamp: None,
                    fence_token: None,
                },
            ),
            (3, results(reference.clone())),
            (
                4,
                SessionLogEntry::cancel_request("stop".into(), "worker".into()),
            ),
            (
                5,
                results(format!("later cid:media:test/session/{}", "b".repeat(64))),
            ),
        ];
        let progress = PublicProgress::from_entries(&entries, 1);
        assert!(progress.available);
        assert_eq!(progress.references, vec![reference.clone()]);
        assert!(!progress
            .output_excerpt
            .as_deref()
            .unwrap()
            .contains("private reasoning"));
        assert!(!progress
            .output_excerpt
            .as_deref()
            .unwrap()
            .contains("later"));
        let result = synthesize_terminated_result(TerminationInputs {
            kind: TerminationKind::Timeout,
            timeout: None,
            public_progress: Some(&progress),
            session_id: "child",
            usage: &Default::default(),
            thinking_excerpt: None,
            budget: None,
            repetition: None,
        });
        assert_eq!(result.termination.thinking_excerpt, None);
        assert!(result.response.contains(&reference));
        assert_eq!(
            result
                .termination
                .public_progress
                .as_ref()
                .unwrap()
                .output_excerpt,
            progress.output_excerpt
        );
    }
}
