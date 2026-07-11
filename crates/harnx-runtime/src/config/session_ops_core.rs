//! Pure session-operation decision helpers shared by local and remote adapters.
use super::*;
use harnx_core::session_reconstruct::active_context_window;

pub(crate) fn adjust_range_for_tool_pairs(
    from: usize,
    to: usize,
    documents: &[String],
) -> Result<(usize, usize)> {
    let parse = |idx: usize| -> Option<SessionLogEntry> {
        documents
            .get(idx)
            .and_then(|raw| serde_yaml::from_str::<SessionLogEntry>(raw).ok())
    };

    if matches!(parse(from), Some(SessionLogEntry::ToolResults { .. })) {
        bail!(
            "Sequence {from} is a tool-results entry; its paired tool-calls entry ({}) \
             would be outside the range. Expand your range to include it.",
            from.saturating_sub(1)
        );
    }

    let mut adjusted_to = to;
    if matches!(parse(to), Some(SessionLogEntry::ToolCalls { .. }))
        && to + 1 < documents.len()
        && matches!(parse(to + 1), Some(SessionLogEntry::ToolResults { .. }))
    {
        adjusted_to = to + 1;
    }

    Ok((from, adjusted_to))
}

pub(crate) fn validate_not_deleting_protected(
    entries: &[SessionLogEntry],
    from: usize,
) -> Result<()> {
    if matches!(entries.get(from), Some(SessionLogEntry::Header { .. })) {
        bail!("Cannot edit or delete the session header (sequence 0)");
    }
    if matches!(entries.get(from), Some(SessionLogEntry::Compress { .. })) {
        bail!("Cannot delete protected session history at or before most-recent boundary");
    }

    let indexed: Vec<_> = entries.iter().cloned().enumerate().collect();
    let window = active_context_window(&indexed);
    if window
        .boundary_index()
        .is_some_and(|boundary| from <= boundary)
    {
        bail!("Cannot delete protected session history at or before most-recent boundary");
    }

    Ok(())
}

pub(crate) fn compute_delete_range(
    from: usize,
    to: usize,
    entries: &[SessionLogEntry],
    documents: &[String],
) -> Result<(usize, usize)> {
    validate_not_deleting_protected(entries, from)?;
    if to >= documents.len() {
        bail!("Sequence numbers out of range");
    }
    let (from, to) = adjust_range_for_tool_pairs(from, to, documents)?;
    if from > to || to >= documents.len() {
        bail!("Sequence numbers out of range");
    }
    Ok((from, to))
}

pub(crate) fn compute_rewind_point(
    after_seq: usize,
    log_entry_count: usize,
    entries: &[SessionLogEntry],
) -> Result<usize> {
    if after_seq >= log_entry_count {
        bail!(
            "Sequence number {} is out of range (log has {} entries)",
            after_seq,
            log_entry_count
        );
    }

    let indexed: Vec<_> = entries.iter().cloned().enumerate().collect();
    let window = active_context_window(&indexed);
    if window
        .boundary_index()
        .is_some_and(|boundary| after_seq <= boundary)
    {
        bail!("Cannot rewind to or before most-recent boundary");
    }

    let parse = |idx: usize| entries.get(idx);
    if matches!(parse(after_seq), Some(SessionLogEntry::ToolCalls { .. }))
        && matches!(
            parse(after_seq + 1),
            Some(SessionLogEntry::ToolResults { .. })
        )
    {
        bail!(
            "Sequence {after_seq} is a tool-calls entry paired with tool-results at {}; \
             rewinding here would orphan the tool calls. \
             Use {} to keep the pair or {} to exclude it.",
            after_seq + 1,
            after_seq + 1,
            after_seq.saturating_sub(1),
        );
    }

    Ok(after_seq)
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::message::{MessageContent, MessageRole};

    fn user(text: &str) -> SessionLogEntry {
        SessionLogEntry::Message {
            id: None,
            timestamp: None,
            fence_token: None,
            role: MessageRole::User,
            content: MessageContent::Text(text.into()),
        }
    }

    fn assistant(text: &str) -> SessionLogEntry {
        SessionLogEntry::Message {
            id: None,
            timestamp: None,
            fence_token: None,
            role: MessageRole::Assistant,
            content: MessageContent::Text(text.into()),
        }
    }

    fn header() -> SessionLogEntry {
        Session::default().build_header_entry()
    }

    fn compress() -> SessionLogEntry {
        SessionLogEntry::Compress {
            prompt: "summary".into(),
        }
    }

    #[test]
    fn validate_not_deleting_protected_rejects_header() {
        let entries = vec![header(), user("u")];
        let err = validate_not_deleting_protected(&entries, 0).expect_err("header protected");
        assert_eq!(
            err.to_string(),
            "Cannot edit or delete the session header (sequence 0)"
        );
    }

    #[test]
    fn validate_not_deleting_protected_rejects_compress_boundary() {
        let entries = vec![user("old"), compress(), user("new")];
        let err = validate_not_deleting_protected(&entries, 1).expect_err("compress protected");
        assert_eq!(
            err.to_string(),
            "Cannot delete protected session history at or before most-recent boundary"
        );
    }

    #[test]
    fn validate_not_deleting_protected_rejects_pre_boundary_history() {
        let entries = vec![user("old"), assistant("older"), compress(), user("new")];
        let err = validate_not_deleting_protected(&entries, 0).expect_err("pre-boundary protected");
        assert_eq!(
            err.to_string(),
            "Cannot delete protected session history at or before most-recent boundary"
        );
    }

    #[test]
    fn validate_not_deleting_protected_allows_post_boundary_turn() {
        let entries = vec![user("old"), compress(), user("new"), assistant("reply")];
        validate_not_deleting_protected(&entries, 2).unwrap();
    }

    #[test]
    fn adjust_range_expands_tool_calls_to_include_results() {
        // documents[1] = ToolCalls, documents[2] = ToolResults.
        let docs = vec![
            serde_yaml::to_string(&header()).unwrap(),
            serde_yaml::to_string(&SessionLogEntry::ToolCalls {
                text: String::new(),
                thought: None,
                calls: vec![],
                timestamp: None,
                fence_token: None,
            })
            .unwrap(),
            serde_yaml::to_string(&SessionLogEntry::ToolResults {
                results: vec![],
                timestamp: None,
            })
            .unwrap(),
        ];
        assert_eq!(adjust_range_for_tool_pairs(1, 1, &docs).unwrap(), (1, 2));
    }

    #[test]
    fn compute_rewind_point_rejects_boundary_or_earlier() {
        let entries = vec![user("old"), compress(), user("new")];
        let err = compute_rewind_point(1, entries.len(), &entries).expect_err("boundary protected");
        assert_eq!(
            err.to_string(),
            "Cannot rewind to or before most-recent boundary"
        );
    }
}
