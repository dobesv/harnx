//! What a tool server advertises, in the two shapes `--list-tools` prints.
//!
//! The JSON form carries the raw [`ToolSpec`]s so a checker can read input
//! schemas and `_meta`, and derive agent-visible names itself from `server`.

use crate::ToolSpec;
use serde::{Deserialize, Serialize};

/// Flag that makes a tool server print its tools and exit.
pub const LIST_TOOLS_FLAG: &str = "--list-tools";
/// Flag choosing the output shape of [`LIST_TOOLS_FLAG`]. Long-only: `-f` is
/// reserved globally by the `harnx` CLI.
pub const LIST_FORMAT_FLAG: &str = "--format";

/// Output shape of `--list-tools`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListingFormat {
    Text,
    Json,
}

/// The tools one server advertises, after any name override and tool filter.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolListing {
    pub server: String,
    pub tools: Vec<ToolSpec>,
}

impl ToolListing {
    /// Human-readable summary: name, first description line and hints per tool.
    pub fn render_text(&self) -> String {
        let mut out = format!(
            "MCP server '{}': {} tool(s)\n",
            self.server,
            self.tools.len()
        );
        for tool in &self.tools {
            out.push_str(&format!("\n  {}\n", tool.name));
            let description = tool.description.trim();
            if !description.is_empty() {
                out.push_str(&format!(
                    "    {}\n",
                    description.lines().next().unwrap_or("")
                ));
            }
            let hints = [
                tool.read_only_hint.then_some("read-only"),
                tool.idempotent_hint.then_some("idempotent"),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
            if !hints.is_empty() {
                out.push_str(&format!("    [{}]\n", hints.join(", ")));
            }
            append_timeout_report(&mut out, tool.timeout_secs);
        }
        if self.tools.is_empty() {
            out.push_str("\n  (the server completed its handshake but advertises no tools)\n");
        }
        out
    }
}

fn append_timeout_report(out: &mut String, timeout_secs: Option<u64>) {
    match timeout_secs {
        Some(0) => out.push_str("    timeout: none\n"),
        Some(seconds) => out.push_str(&format!("    timeout: {seconds}s\n")),
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CancellationGuarantee;
    use serde_json::json;

    fn spec(name: &str, description: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_string(),
            description: description.to_string(),
            input_schema: json!({"type": "object"}),
            cancellation_guarantee: CancellationGuarantee::Cooperative,
            idempotent_hint: true,
            read_only_hint: true,
            timeout_secs: Some(0),
            meta: None,
        }
    }

    fn listing() -> ToolListing {
        ToolListing {
            server: "time".to_string(),
            tools: vec![spec("wait", "Wait a while.\nMore."), spec("now", "")],
        }
    }

    #[test]
    fn listing_round_trips_through_json() {
        let listing = listing();
        let json = serde_json::to_string_pretty(&listing).unwrap();
        let back: ToolListing = serde_json::from_str(&json).unwrap();
        assert_eq!(back, listing);
    }

    #[test]
    fn render_text_matches_bridge_format() {
        let text = listing().render_text();
        assert!(text.starts_with("MCP server 'time': 2 tool(s)\n"), "{text}");
        assert!(text.contains("\n  wait\n    Wait a while.\n"), "{text}");
        assert!(!text.contains("More."), "{text}");
        assert!(text.contains("\n  now\n"), "{text}");
        assert!(text.contains("    [read-only, idempotent]\n"), "{text}");
        assert!(text.contains("    timeout: none\n"), "{text}");
    }

    #[test]
    fn timeout_report_distinguishes_disabled_explicit_and_default_timeouts() {
        let mut report = String::new();
        append_timeout_report(&mut report, Some(0));
        append_timeout_report(&mut report, Some(45));
        append_timeout_report(&mut report, None);
        assert_eq!(report, "    timeout: none\n    timeout: 45s\n");
    }
}
