//! Session-log parsing/validation helpers extracted from config/mod.rs for code health.
use super::*;

pub(crate) fn split_session_log_documents(raw_log: &str) -> Vec<String> {
    // Normalize Windows line endings before splitting so that session files
    // transferred between platforms are handled correctly.
    let normalized = if raw_log.contains("\r\n") {
        std::borrow::Cow::Owned(raw_log.replace("\r\n", "\n"))
    } else {
        std::borrow::Cow::Borrowed(raw_log)
    };
    normalized
        .split("\n---\n")
        .filter_map(|document| {
            let document = document.trim();
            let document = document.strip_prefix("---\n").unwrap_or(document).trim();
            if document.is_empty() {
                None
            } else {
                Some(document.to_string())
            }
        })
        .collect()
}

pub(crate) fn validate_edited_session_documents(content: &str) -> Result<Vec<String>> {
    let documents = split_session_log_documents(content);
    for document in &documents {
        serde_yaml::from_str::<SessionLogEntry>(document).with_context(|| {
            format!(
                "Invalid session log entry YAML:
{document}"
            )
        })?;
    }
    Ok(documents)
}

/// Adjust `[from, to]` so that `ToolCalls`/`ToolResults` pairs are never
/// split across the range boundary, then return the (possibly expanded) range.
///
/// Rules:
/// - If `from` points at a `ToolResults` entry (i.e. the pair's `ToolCalls` is
///   at `from - 1`, outside the range), that is an error: we can't silently
///   expand backward because the caller's intent is unclear.
/// - If `to` points at a `ToolCalls` entry and `to + 1` is its paired
///   `ToolResults`, auto-expand `to` by one.
///
/// Returns `(adjusted_from, adjusted_to)`.
pub(crate) fn adjust_range_for_tool_pairs(
    from: usize,
    to: usize,
    documents: &[String],
) -> Result<(usize, usize)> {
    // Parse only the entries we need: the one just before `from` (to check if
    // `from` is a dangling ToolResults) and up through `to + 1` (to check if
    // `to` is a ToolCalls that needs its partner).
    let parse = |idx: usize| -> Option<SessionLogEntry> {
        documents
            .get(idx)
            .and_then(|raw| serde_yaml::from_str::<SessionLogEntry>(raw).ok())
    };

    // Reject: range starts on a ToolResults whose ToolCalls is outside the range.
    if matches!(parse(from), Some(SessionLogEntry::ToolResults { .. })) {
        // Check if the immediately preceding entry is a ToolCalls — if so,
        // this is definitely a dangling-results situation.
        bail!(
            "Sequence {from} is a tool-results entry; its paired tool-calls entry ({}) \
             would be outside the range. Expand your range to include it.",
            from.saturating_sub(1)
        );
    }

    // Auto-expand: range ends on a ToolCalls whose ToolResults is just outside.
    let mut adjusted_to = to;
    if matches!(parse(to), Some(SessionLogEntry::ToolCalls { .. }))
        && to + 1 < documents.len()
        && matches!(parse(to + 1), Some(SessionLogEntry::ToolResults { .. }))
    {
        adjusted_to = to + 1;
    }

    Ok((from, adjusted_to))
}

pub(crate) fn validate_tool_pair_integrity(start_seq: usize, documents: &[String]) -> Result<()> {
    let entries = documents
        .iter()
        .map(|document| {
            serde_yaml::from_str::<SessionLogEntry>(document).with_context(|| {
                format!(
                    "Invalid session log entry YAML:
{document}"
                )
            })
        })
        .collect::<Result<Vec<_>>>()?;

    for (index, entry) in entries.iter().enumerate() {
        let SessionLogEntry::ToolCalls { calls, .. } = entry else {
            continue;
        };

        let Some(SessionLogEntry::ToolResults { results, .. }) = entries.get(index + 1) else {
            let call_seq = start_seq + index;
            bail!(
                "Edited tool call entry at {call_seq} must be followed immediately by matching tool results"
            );
        };

        let call_ids: HashSet<_> = calls.iter().filter_map(|call| call.id.as_deref()).collect();
        let result_seq = start_seq + index + 1;
        let missing_result_ids = results
            .iter()
            .filter(|result| result.id.as_deref().is_none_or(str::is_empty))
            .count();

        if missing_result_ids == results.len() {
            if results.len() != calls.len() {
                bail!(
                    "Edited tool result at {result_seq} is missing tool_call_id for positional matching and count {} does not match tool calls count {}",
                    results.len(),
                    calls.len()
                );
            }
            continue;
        }

        if missing_result_ids > 0 {
            bail!(
                "Edited tool result at {result_seq} mixes tool_call_id values with missing tool_call_id entries"
            );
        }

        // All results have IDs: enforce strict 1:1 mapping.
        // Collect in order so duplicate detection and count check work together.
        let result_ids: Vec<&str> = results
            .iter()
            .map(|r| r.id.as_deref().filter(|id| !id.is_empty()).unwrap())
            .collect();

        if result_ids.len() != calls.len() {
            let expected_ids = call_ids.iter().copied().collect::<Vec<_>>().join(", ");
            bail!(
                "Edited tool result at {result_seq} has {} result(s) but {} call(s) (expected ids: {expected_ids})",
                result_ids.len(),
                calls.len()
            );
        }

        let result_id_set: HashSet<&str> = result_ids.iter().copied().collect();
        if result_id_set.len() != result_ids.len() {
            bail!("Edited tool result at {result_seq} contains duplicate tool_call_id values");
        }

        for call_id in &result_ids {
            if !call_ids.contains(call_id) {
                let expected_ids = call_ids.iter().copied().collect::<Vec<_>>().join(", ");
                bail!(
                    "Edited tool result at {result_seq} references unknown tool_call_id '{call_id}' (expected one of: {expected_ids})"
                );
            }
        }
    }

    Ok(())
}
