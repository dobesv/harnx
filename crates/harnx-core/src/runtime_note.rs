//! Structured identity for runtime-authored context notes.
//!
//! Replay synthesizes user-role context notes from control events (sub-agent
//! starts, cancels) so the model sees them in conversation order. Those notes
//! are not prompts: nobody admitted them, and they must never be folded as
//! new input or treated as pending work. Before this module existed the
//! in-memory notes carried no id, so a compaction re-log persisted them as
//! plain ID-less user rows that looked exactly like prompts to every
//! admission check — the source of "injected user row has no admission
//! identity" failures on sessions that compacted mid-turn (issue #2493).
//!
//! Synthesized notes now carry a `runtime-note-v1-<uuid>` message id. The id
//! is assigned by worker-internal synthesis, and prompt admission reserves the
//! prefix ([`is_reserved_message_id`]), so within harnx's own append paths the
//! prefix is a provenance marker that survives compaction's verbatim re-log.
//!
//! The classifier does not authenticate: a principal that can already publish
//! raw records to a session log can forge a runtime-note id and hide its row
//! from prompt admission and pending-input checks (its content still replays
//! into model history). Such a principal can already inject history under
//! ordinary ids, so this adds stealth, not capability; raw-log publishers are
//! trusted, the same model as the timing-notice id.
//!
//! # Why no text-prefix fallback
//!
//! Only the structured id classifies a row as a runtime note. The
//! human-readable `[Runtime note]` prefix is deliberately NOT trusted as
//! provenance: user-authored prompts may legitimately start with it, and the
//! repository's timing-notice tests pin the invariant that a prompt merely
//! mimicking a reserved marker stays executable (see
//! `timing_non_executable_tests`). Legacy ID-less note rows written by the
//! pre-identity compaction bug therefore still fail closed at admission
//! verification instead of being silently skipped.

/// Prefix of the human-readable runtime-note text. Never used as a
/// classification signal — see the module docs.
pub const RUNTIME_NOTE_PREFIX: &str = "[Runtime note] ";

/// Prefix of the structured message id assigned to synthesized runtime notes.
pub const RUNTIME_NOTE_ID_PREFIX: &str = "runtime-note-v1-";

/// Fresh identity for a synthesized runtime note. Unique per note: two
/// identical cancel notes in one session are separate context rows.
pub fn new_runtime_note_message_id() -> String {
    format!("{RUNTIME_NOTE_ID_PREFIX}{}", uuid::Uuid::new_v4())
}

/// Whether a message id was minted by runtime-note synthesis.
pub fn is_runtime_note_message_id(id: &str) -> bool {
    id.starts_with(RUNTIME_NOTE_ID_PREFIX)
}

/// Whether a durable user row is runtime-authored context rather than an
/// executable prompt. Structured id only — see the module docs for why the
/// text prefix is not a signal.
pub fn is_runtime_note_id(id: Option<&str>) -> bool {
    id.is_some_and(is_runtime_note_message_id)
}

/// Whether a caller-supplied message id collides with a reserved structured
/// identity. Admission entry points that accept caller ids must reject these:
/// a prompt admitted under a reserved prefix would be silently skipped by
/// every executable-prompt check. Internal compaction re-logs copy ids
/// verbatim and never pass through admission, so they are unaffected.
pub fn is_reserved_message_id(id: &str) -> bool {
    is_runtime_note_message_id(id)
        || id.starts_with(crate::timing_identity::TIMING_MESSAGE_ID_PREFIX)
}

/// Compact one-line display form of a runtime note, for UI rendering.
/// Returns `None` for text that is not a runtime note.
///
/// Every `[Runtime note]` kind gets a short line: timing notices become
/// `⏱ 526m 40s elapsed · 913m 19s left of 1440m`, sub-agent starts, turn
/// interruptions and compaction summaries get a short phrase, and an unknown
/// note falls back to a truncated first line. The raw text still reaches the
/// model unchanged; only UI rendering uses this.
///
/// Tradeoff for callers: unlike the id-based checks above, this classifies by
/// content. The no-text-prefix rule in the module docs exists to protect
/// admission and execution decisions, where a lookalike prompt must stay
/// executable — rendering is not such a decision, so a user prompt that
/// mimics this format still executes and still reaches the model verbatim.
/// What it loses is display fidelity only: it renders as a dim one-liner
/// (multi-line bodies truncate to the first line) with no markdown-link rows
/// and no edit/rewind affordances. Callers that have the durable message id
/// should classify id-first and treat non-reserved ids as prompts (the TUI
/// history-replay path does this); id-less callers — live user events carry
/// only content — must accept the tradeoff above.
pub fn compact_display(text: &str) -> Option<String> {
    if let Some(timing) = crate::timing_identity::compact_timing_display(text) {
        return Some(timing);
    }
    let body = text.strip_prefix(RUNTIME_NOTE_PREFIX)?;
    Some(compact_prose(body))
}

/// Compact form of a non-timing runtime note (the prefix is already stripped).
fn compact_prose(body: &str) -> String {
    if let Some(rest) = body.strip_prefix("Started sub-agent '") {
        if let Some((agent, tail)) = rest.split_once("' in session ") {
            let session_id = tail.strip_suffix('.').unwrap_or(tail);
            return format!("Sub-agent '{agent}' started (session {session_id})");
        }
    }
    if body.starts_with("The user interrupted this turn") {
        return "Turn interrupted; incomplete tool calls cancelled".to_string();
    }
    if body.strip_prefix("Earlier conversation summary:").is_some() {
        return "Earlier conversation summary".to_string();
    }
    truncated_first_line(body)
}

/// First line of an unrecognized note, cut to a display-friendly length.
fn truncated_first_line(body: &str) -> String {
    const MAX_CHARS: usize = 80;
    let first = body.lines().next().unwrap_or("");
    if first.chars().count() <= MAX_CHARS {
        return first.to_string();
    }
    let mut truncated: String = first.chars().take(MAX_CHARS).collect();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_id_is_recognized_regardless_of_text() {
        let id = new_runtime_note_message_id();
        assert!(is_runtime_note_message_id(&id));
        assert!(is_runtime_note_id(Some(&id)));
    }

    #[test]
    fn every_generated_id_is_unique() {
        let a = new_runtime_note_message_id();
        let b = new_runtime_note_message_id();
        assert_ne!(a, b);
    }

    #[test]
    fn other_ids_and_idless_rows_are_not_runtime_notes() {
        // Admitted prompts carry admission ids; a user prompt that happens to
        // start with the note prefix keeps its id and stays executable.
        let admitted = uuid::Uuid::new_v4().to_string();
        assert!(!is_runtime_note_id(Some(&admitted)));
        // ID-less rows are never classified by text, matching the repo
        // invariant that reserved-prefix lookalikes stay executable.
        assert!(!is_runtime_note_id(None));
    }

    #[test]
    fn reserved_ids_cover_runtime_note_and_timing_prefixes() {
        assert!(is_reserved_message_id(&new_runtime_note_message_id()));
        assert!(is_reserved_message_id("timing-v1-abc123"));
        // Ordinary caller ids (uuids) stay admissible.
        assert!(!is_reserved_message_id(&uuid::Uuid::new_v4().to_string()));
        // The human-readable note prefix in an id is not reserved by itself.
        assert!(!is_reserved_message_id(RUNTIME_NOTE_PREFIX.trim_end()));
    }

    #[test]
    fn non_notes_have_no_compact_form() {
        assert_eq!(compact_display("What is the capital of France?"), None);
        assert_eq!(compact_display(""), None);
        assert_eq!(compact_display("[Runtime note]"), None);
        // Missing trailing space: not the note prefix, so not a note.
        assert_eq!(compact_display("[Runtime note]no space"), None);
    }

    #[test]
    fn timing_notice_compacts_to_elapsed_and_remaining() {
        let text = "[Runtime note] ⏱ v1 {\"version\":\"v1\",\"origin\":\"2026-11-12T20:40:32Z\",\
\"invocation_id\":\"ca615ba5\",\"coverage\":1,\"highest_elapsed_bucket\":35} | \
start: 2026-11-12T20:40:32Z, current: 2026-11-13T05:27:12Z, elapsed: 526m 40s, \
deadline: 2026-11-13T20:40:32Z, remaining: 913m 19s, budget: 1440m";
        assert_eq!(
            compact_display(text).as_deref(),
            Some("⏱ 526m 40s elapsed · 913m 19s left of 1440m")
        );
    }

    #[test]
    fn timing_notice_without_deadline_compacts_to_elapsed_only() {
        let text = "[Runtime note] ⏱ v1 {\"version\":\"v1\",\"origin\":\"2026-11-13T06:00:00Z\",\
\"invocation_id\":\"inv\",\"coverage\":1,\"highest_elapsed_bucket\":0} | \
start: 2026-11-13T06:00:00Z, current: 2026-11-13T06:10:00Z, elapsed: 10m";
        assert_eq!(compact_display(text).as_deref(), Some("⏱ 10m elapsed"));
    }

    #[test]
    fn timing_notice_thresholds_survive_compacting() {
        let build = |coverage: u32| {
            format!(
                "[Runtime note] ⏱ v1 {{\"version\":\"v1\",\"origin\":\"2026-11-13T06:00:00Z\",\
\"invocation_id\":\"inv\",\"coverage\":{coverage},\"highest_elapsed_bucket\":2}} | \
start: 2026-11-13T06:00:00Z, current: 2026-11-13T18:00:00Z, elapsed: 720m, \
deadline: 2026-11-14T06:00:00Z, remaining: 720m, budget: 1440m"
            )
        };
        // Coverage bitmask: 1 = initial, 2 = 50%, 4 = 75%, 8 = 90%.
        assert_eq!(
            compact_display(&build(1 | 2)).as_deref(),
            Some("⏱ 720m elapsed · 720m left of 1440m · 50% threshold passed")
        );
        assert_eq!(
            compact_display(&build(1 | 2 | 4)).as_deref(),
            Some("⏱ 720m elapsed · 720m left of 1440m · 75% threshold passed")
        );
        assert_eq!(
            compact_display(&build(1 | 2 | 4 | 8)).as_deref(),
            Some("⏱ 720m elapsed · 720m left of 1440m · 90% threshold passed")
        );
    }

    #[test]
    fn sub_agent_start_note_compacts() {
        let text = "[Runtime note] Started sub-agent 'pantheon/plato' in session child-direct.";
        assert_eq!(
            compact_display(text).as_deref(),
            Some("Sub-agent 'pantheon/plato' started (session child-direct)")
        );
    }

    #[test]
    fn cancel_note_compacts() {
        let text = "[Runtime note] The user interrupted this turn. Incomplete tool calls above were cancelled.";
        assert_eq!(
            compact_display(text).as_deref(),
            Some("Turn interrupted; incomplete tool calls cancelled")
        );
    }

    #[test]
    fn compaction_summary_note_compacts_without_summary_body() {
        let text =
            "[Runtime note] Earlier conversation summary:\n\nPRIOR WORK SUMMARY line one\nline two";
        assert_eq!(
            compact_display(text).as_deref(),
            Some("Earlier conversation summary")
        );
    }

    #[test]
    fn unknown_note_falls_back_to_truncated_first_line() {
        let short = "[Runtime note] Some future note kind.";
        assert_eq!(
            compact_display(short).as_deref(),
            Some("Some future note kind.")
        );

        let long_first_line = format!("[Runtime note] {}", "x".repeat(100));
        let compact = compact_display(&long_first_line).expect("compact form");
        assert_eq!(compact.chars().count(), 81);
        assert!(compact.ends_with('…'));
        // Only the first line is kept, even when more lines follow.
        let multi_line = "[Runtime note] first line of an unknown note\nsecond line";
        assert_eq!(
            compact_display(multi_line).as_deref(),
            Some("first line of an unknown note")
        );
    }
}
