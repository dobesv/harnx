use crate::HookEvent;

use anyhow::{Context, Result};

pub struct CompiledMatcher {
    regex: Option<fancy_regex::Regex>,
}

impl CompiledMatcher {
    pub fn compile(pattern: &Option<String>) -> Result<Self> {
        let regex = pattern
            .as_ref()
            .map(|pattern| {
                fancy_regex::Regex::new(pattern)
                    .with_context(|| format!("failed to compile hook matcher regex: {pattern}"))
            })
            .transpose()?;

        Ok(Self { regex })
    }

    pub fn matches(&self, event: &HookEvent) -> bool {
        match &self.regex {
            None => true,
            Some(regex) => event
                .matcher_text()
                .and_then(|text| regex.is_match(text).ok())
                .unwrap_or(false),
        }
    }

    /// Match against an explicit text string instead of extracting from an event.
    /// Used for server-scoped hooks where the matcher applies to the bare (unprefixed)
    /// tool name rather than the display name carried in the event.
    pub fn matches_str(&self, text: &str) -> bool {
        match &self.regex {
            None => true,
            Some(regex) => regex.is_match(text).unwrap_or(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::CompiledMatcher;
    use crate::HookEvent;
    use serde_json::json;

    // --- matches_str tests ---------------------------------------------------

    #[test]
    fn test_matches_str_no_pattern_matches_all() {
        let matcher = CompiledMatcher::compile(&None).expect("compile empty matcher");
        assert!(matcher.matches_str("anything"));
        assert!(matcher.matches_str(""));
    }

    #[test]
    fn test_matches_str_exact_match() {
        let matcher = CompiledMatcher::compile(&Some("^exec$".to_string())).expect("compile regex");
        assert!(matcher.matches_str("exec"));
        assert!(!matcher.matches_str("bash_exec"));
        assert!(!matcher.matches_str("exec_more"));
    }

    #[test]
    fn test_matches_str_partial_pattern() {
        let matcher = CompiledMatcher::compile(&Some("exec".to_string())).expect("compile regex");
        // "exec" as a substring regex matches both
        assert!(matcher.matches_str("exec"));
        assert!(matcher.matches_str("bash_exec"));
    }

    #[test]
    fn test_matches_str_no_match() {
        let matcher = CompiledMatcher::compile(&Some("^list".to_string())).expect("compile regex");
        assert!(!matcher.matches_str("exec"));
        assert!(matcher.matches_str("list_files"));
    }

    #[test]
    fn test_matcher_no_pattern() {
        let matcher = CompiledMatcher::compile(&None).expect("compile empty matcher");

        assert!(matcher.matches(&HookEvent::SessionStart {
            source: "cli".to_string(),
            model: "claude".to_string(),
        }));
    }

    #[test]
    fn test_matcher_matches_tool_name() {
        let matcher =
            CompiledMatcher::compile(&Some("execute_command".to_string())).expect("compile regex");

        assert!(matcher.matches(&HookEvent::PreToolUse {
            tool_name: "execute_command".to_string(),
            tool_input: json!({"command": "pwd"}),
            tool_use_id: "call-1".to_string(),
        }));
    }

    #[test]
    fn test_matcher_no_match() {
        let matcher = CompiledMatcher::compile(&Some("shell".to_string())).expect("compile regex");

        assert!(!matcher.matches(&HookEvent::PreToolUse {
            tool_name: "web_search".to_string(),
            tool_input: json!({"query": "rust"}),
            tool_use_id: "call-1".to_string(),
        }));
    }

    #[test]
    fn test_matcher_non_tool_event() {
        let matcher = CompiledMatcher::compile(&Some("shell".to_string())).expect("compile regex");

        assert!(!matcher.matches(&HookEvent::SessionStart {
            source: "cli".to_string(),
            model: "claude".to_string(),
        }));
    }
}
