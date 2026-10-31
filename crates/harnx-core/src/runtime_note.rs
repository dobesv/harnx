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
}
