//! Non-modal A2UI surface publication and action submission (plan T5.1).
//!
//! A completed tool result can carry user-audience A2UI surface envelopes in
//! `ToolResults` (`ToolOutput::user_surfaces`); they never reach the model.
//! Frontends render them and submit actions through
//! [`crate::nats_session::NatsSession::submit_surface_action`]. An action's
//! visibility decides its effect:
//!
//! - `invisible`: the durable `SurfaceAction` record is the whole effect.
//! - `note`: a passive context line at the next model call, no new turn.
//! - `turn`: the action's user message starts a turn, or joins the running one.
//!
//! Actions declaring `harnx.tool` route through the operator tool RPC, so the
//! agent's tool permissions and middleware apply. No path synthesizes a model
//! tool call.

use anyhow::Result;
use harnx_core::interaction::non_modal::{self, SurfaceActionDeclaration, SurfaceActionError};
use harnx_core::interaction::InteractionResponse;
use harnx_core::session::SessionLogEntry;
use harnx_core::session_reconstruct::apply_log_mutations_nats;
mod tool;

use serde::{Deserialize, Serialize};

use serde_json::Value;
pub use tool::{run_surface_tool, SurfaceToolRoute};

/// Bounded passes over the CAS conflict race before a surface action append is
/// failed. Shared by the lease-holder worker path and the client idle path so
/// both fail identically. A turn that is actively appending (tool calls and
/// results) moves the tail constantly, so a tight handful of retries can lose
/// every race; the budget is generous and each retry backs off. The
/// `HARNX_SURFACE_ACTION_CAS_ROUNDS` override is a test knob for
/// deterministic exhaustion; production keeps the default.
pub const APPEND_CONFLICT_ROUNDS: usize = 25;

/// Pause between CAS conflict rounds, so a hot log tail and the retry
/// loop are not locked into a spin where every retry loses.
pub const APPEND_CONFLICT_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(20);

/// The effective CAS conflict budget, from [`APPEND_CONFLICT_ROUNDS`] or the
/// test-only `HARNX_SURFACE_ACTION_CAS_ROUNDS` override.
pub fn append_conflict_rounds() -> usize {
    std::env::var("HARNX_SURFACE_ACTION_CAS_ROUNDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|rounds| *rounds > 0)
        .unwrap_or(APPEND_CONFLICT_ROUNDS)
}

/// A frontend's action on one published surface.
///
/// Identity is `(surface_id, publish_seq)`: the envelope's `surfaceId` plus the
/// log sequence of the `ToolResults` entry that published it. A surface is
/// actionable only from that durable entry, never from in-flight output, so a
/// model cannot forge one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SurfaceActionSubmission {
    pub surface_id: String,
    pub publish_seq: u64,
    pub response: InteractionResponse,
    /// The action's `userMessage`; wins over the generated text for
    /// `note`/`turn` visibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_message: Option<String>,
}

/// Result of evaluating and applying a surface action.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceActionOutcome {
    /// Record appended (fenced by the lease holder or CAS by the client).
    Applied,
    /// Identical action was already durable; acknowledged without append.
    AlreadyApplied,
    /// A different action was already accepted for this surface generation.
    Conflicting,
    /// No durable `ToolResults` publishes this `(surface_id, publish_seq)`.
    UnknownSurface,
    /// Not a Button action declared on this surface (`deny` included).
    UndeclaredAction,
    /// The declaration names a visibility outside the three allowed values.
    InvalidVisibility,
    /// The declaration names a malformed direct tool target.
    InvalidTool,
    /// Submitted `data` does not match the surface's bound inputs.
    InvalidData,
    /// The declared target tool is not available under the session's tool
    /// permissions. Nothing was executed or recorded.
    UnauthorizedTool,
    /// The declared target tool ran and failed. Nothing was recorded, so a
    /// retry re-runs the tool (the documented execute-then-record window).
    ToolFailed,
    /// The lease is not held or the fence CAS failed.
    LeaseLost,
    /// The action validated and its tool (if any) ran once, but the durable
    /// record could not be appended — bounded CAS retries against a log tail
    /// that kept moving. Nothing is durable, so a resubmit is safe; if a tool
    /// ran, its result is quoted only once the record lands. Also the
    /// containment answer when the worker's handler panicked after
    /// acknowledging (review N-B): the same nothing-recorded contract, with
    /// the panic logged worker-side; a resubmission re-validates from the
    /// log and re-runs any tool under the documented execute-then-record
    /// window.
    AppendFailed,
    /// Wire-only early acknowledgement: the lease-holding worker received the
    /// command and is processing it. Never returned by
    /// [`NatsSession::submit_surface_action`]; the client consumes it as
    /// "a worker owns this attempt — never fall back to idle execution".
    Pending,
}

impl SurfaceActionOutcome {
    /// True when the action was accepted (newly applied or idempotent retry).
    pub fn is_applied(&self) -> bool {
        matches!(self, Self::Applied | Self::AlreadyApplied)
    }
}

/// One published surface found in a durable `ToolResults` entry.
#[derive(Clone, Debug, PartialEq)]
pub struct PublishedSurface {
    /// The envelope exactly as published (`createSurface.surfaceId` matches).
    pub payload: Value,
    /// The tool call that produced the surface, for frontend display.
    pub tool_call_id: Option<String>,
}

/// Every user-audience surface published by effective `ToolResults` entries,
/// in log order, with the publishing entry's sequence.
pub fn published_surfaces(
    entries: &[(u64, SessionLogEntry)],
) -> Result<Vec<(u64, PublishedSurface)>> {
    let effective = apply_log_mutations_nats(entries)?;
    let mut surfaces = Vec::new();
    for (seq, entry) in &effective {
        let SessionLogEntry::ToolResults { results, .. } = entry else {
            continue;
        };
        for output in results {
            for payload in &output.user_surfaces {
                surfaces.push((
                    *seq,
                    PublishedSurface {
                        payload: payload.clone(),
                        tool_call_id: output.id.clone(),
                    },
                ));
            }
        }
    }
    Ok(surfaces)
}

/// The published surface named by `(surface_id, publish_seq)`, if any.
pub fn find_published_surface(
    entries: &[(u64, SessionLogEntry)],
    surface_id: &str,
    publish_seq: u64,
) -> Result<Option<PublishedSurface>> {
    Ok(published_surfaces(entries)?
        .into_iter()
        .find(|(seq, surface)| {
            *seq == publish_seq && surface_id_of(&surface.payload).as_deref() == Some(surface_id)
        })
        .map(|(_, surface)| surface))
}

/// The already-accepted action for this surface generation, if any. First-wins:
/// an identical retry stays `already_applied`; any difference is `conflicting`.
pub fn existing_surface_action(
    entries: &[(u64, SessionLogEntry)],
    surface_id: &str,
    publish_seq: u64,
) -> Option<AcceptedSurfaceAction> {
    entries.iter().rev().find_map(|(_, entry)| match entry {
        SessionLogEntry::SurfaceAction {
            surface_id: id,
            publish_seq: seq,
            action,
            data,
            visibility,
            note_text,
            ..
        } => (id == surface_id && *seq == publish_seq).then(|| AcceptedSurfaceAction {
            action: action.clone(),
            data: data.clone(),
            visibility: visibility.clone(),
            note_text: note_text.clone(),
        }),
        _ => None,
    })
}

/// The durable `SurfaceAction` record for one accepted action, as read back
/// from the log. `visibility` and `note_text` let a retry recover the
/// `turn`-visibility effect when the original reply was lost (review C7).
#[derive(Clone, Debug, PartialEq)]
pub struct AcceptedSurfaceAction {
    pub action: String,
    pub data: Value,
    pub visibility: String,
    pub note_text: Option<String>,
}

impl AcceptedSurfaceAction {
    /// Whether this record was accepted with `turn` visibility.
    pub fn is_turn(&self) -> bool {
        self.visibility == non_modal::VISIBILITY_TURN
    }

    /// Whether this submission matches this record's action and data, so the
    /// retry is an idempotent `already_applied` rather than a `conflicting`
    /// submit of a different action.
    pub fn matches(&self, submission: &SurfaceActionSubmission) -> bool {
        self.action == submission.response.action
            && self.data == Value::Object(submission.response.data.clone())
    }
}

/// Validate a submission against the durable surface, returning its
/// declaration. Every `Err` outcome — rejections plus `already_applied` and
/// `conflicting` — means nothing may be appended, dispatched or enqueued;
/// only `Ok` proceeds to effects.
pub fn validate_surface_submission(
    entries: &[(u64, SessionLogEntry)],
    submission: &SurfaceActionSubmission,
) -> Result<SurfaceActionDeclaration, SurfaceActionOutcome> {
    if let Some(accepted) =
        existing_surface_action(entries, &submission.surface_id, submission.publish_seq)
    {
        return Err(if accepted.matches(submission) {
            SurfaceActionOutcome::AlreadyApplied
        } else {
            SurfaceActionOutcome::Conflicting
        });
    }
    let surface = find_published_surface(entries, &submission.surface_id, submission.publish_seq)
        .map_err(|_| SurfaceActionOutcome::UnknownSurface)?
        .ok_or(SurfaceActionOutcome::UnknownSurface)?;
    match non_modal::validate_surface_action(&surface.payload, &submission.response) {
        Ok(declaration) => Ok(declaration),
        Err(SurfaceActionError::UndeclaredAction) => Err(SurfaceActionOutcome::UndeclaredAction),
        Err(SurfaceActionError::InvalidVisibility(_)) => {
            Err(SurfaceActionOutcome::InvalidVisibility)
        }
        Err(SurfaceActionError::InvalidTool(_)) => Err(SurfaceActionOutcome::InvalidTool),
        Err(SurfaceActionError::InvalidData(_)) => Err(SurfaceActionOutcome::InvalidData),
    }
}

/// The durable record for an accepted action.
pub fn surface_action_entry(
    submission: &SurfaceActionSubmission,
    declaration: &SurfaceActionDeclaration,
    note_text: Option<&str>,
    fence_token: u64,
) -> SessionLogEntry {
    // One clone of the response carries action, data and note together;
    // field-by-field cloning hides that they share the submitted response.
    let response = submission.response.clone();
    SessionLogEntry::SurfaceAction {
        surface_id: submission.surface_id.clone(),
        publish_seq: submission.publish_seq,
        action: response.action,
        data: Value::Object(response.data),
        note: response.note,
        user_message: submission.user_message.clone(),
        visibility: declaration.visibility.as_str().to_string(),
        note_text: note_text.map(str::to_string),
        fence_token,
    }
}

/// Fold `note`-visibility action records into one passive context string for
/// the next turn's first model call.
///
/// A note counts as pending until it has actually been delivered: delivered
/// notes are durable hook-context message rows (issue #2198), so a record is
/// excluded once a hook-context row containing its text exists. Mid-turn
/// submissions are pushed straight into the running loop's queue and get
/// their row during the turn; notes submitted while no turn is running (or
/// whose queue died with an unfinished activation) are picked up here. The
/// position of the record relative to `TurnEnd` cannot be used: a note
/// submitted between a turn's last model call and its `TurnEnd` record would
/// fall outside both mechanisms.
pub fn fold_pending_surface_notes(entries: &[(u64, SessionLogEntry)]) -> Option<String> {
    let delivered: Vec<String> = entries
        .iter()
        .filter_map(|(_, entry)| {
            let SessionLogEntry::Message {
                id: Some(id),
                content,
                ..
            } = entry
            else {
                return None;
            };
            harnx_core::runtime_note::is_hook_context_id(Some(id)).then(|| content.to_text())
        })
        .collect();
    let notes: Vec<&str> = entries
        .iter()
        .filter_map(|(_, entry)| {
            let SessionLogEntry::SurfaceAction {
                visibility,
                note_text,
                ..
            } = entry
            else {
                return None;
            };
            if visibility != non_modal::VISIBILITY_NOTE {
                return None;
            }
            let note_text = note_text.as_deref()?;
            if note_text.is_empty() || delivered.iter().any(|row| row.contains(note_text)) {
                return None;
            }
            Some(note_text)
        })
        .collect();
    (!notes.is_empty()).then(|| notes.join("\n\n"))
}

/// The `createSurface.surfaceId` of a surface envelope.
pub fn surface_id_of(payload: &Value) -> Option<String> {
    payload
        .pointer("/createSurface/surfaceId")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// The worker's live-side handles for surface actions: the running agent
/// loop's pending-context queue (the #2198 `hook_context` path) plus whether a
/// turn is currently executing on this activation.
///
/// Mid-turn `note` actions push into the queue so the next model call in the
/// running turn sees them without a new turn starting. When no turn is
/// running the note stays durable-only and the next turn's
/// [`fold_pending_surface_notes`] delivers it, so no record is ever delivered
/// twice.
#[derive(Clone, Debug)]
pub struct SurfaceActionQueue {
    notes: std::sync::Arc<tokio::sync::Mutex<Option<String>>>,
    turn_running: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Default for SurfaceActionQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl SurfaceActionQueue {
    pub fn new() -> Self {
        Self {
            notes: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            turn_running: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// The loop-side queue handle, for `AgentLoopContext::surface_notes`.
    pub fn pending_context(&self) -> std::sync::Arc<tokio::sync::Mutex<Option<String>>> {
        self.notes.clone()
    }

    pub fn set_turn_running(&self, running: bool) {
        self.turn_running
            .store(running, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn is_turn_running(&self) -> bool {
        self.turn_running.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Queue a note for the running turn's next model call. No-op outside a
    /// turn: the durable record plus the turn-start fold covers that case.
    pub async fn push_note(&self, note: &str) {
        if !self.is_turn_running() {
            return;
        }
        let mut queue = self.notes.lock().await;
        match queue.as_mut() {
            Some(pending) => {
                pending.push_str("\n\n");
                pending.push_str(note);
            }
            None => *queue = Some(note.to_string()),
        }
    }
}

/// The lease holder's answer to a submitted surface action.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SurfaceActionReply {
    pub outcome: SurfaceActionOutcome,
    /// On `applied` with `turn` visibility: the text the client should
    /// enqueue as the user message that starts (or joins) the turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_text: Option<String>,
}

impl SurfaceActionReply {
    pub fn rejected(outcome: SurfaceActionOutcome) -> Self {
        Self {
            outcome,
            turn_text: None,
        }
    }
}

#[cfg(test)]
mod tests;
