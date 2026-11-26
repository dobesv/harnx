//! Non-modal A2UI surfaces in the TUI (plan T5.1, Part B).
//!
//! Surfaces are durable: a surface is actionable only from the `ToolResults`
//! entry that published it (Part A contract, note `t5-1-part-a-api`). This
//! module watches the session log — the same pattern as
//! `interaction_confirmation`'s observer — and mirrors the durable surface
//! state into the transcript and the action form. Nothing here suspends or
//! blocks a turn: the composer stays usable while surfaces are outstanding.

use crate::types::{ModalState, TranscriptItem, Tui, TuiEvent};
use harnx_core::interaction::InteractionResponse;
use harnx_runtime::surfaces::{SurfaceActionOutcome, SurfaceActionSubmission};
use std::collections::HashMap;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

mod observe;
pub(crate) mod render;
#[cfg(test)]
mod tests;

pub(super) type Target = (String, String);

/// One durable published surface, with its accepted action if any.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SurfaceSnapshot {
    pub surface_id: String,
    pub publish_seq: u64,
    pub payload: serde_json::Value,
    /// The accepted action name once a submission was durably recorded
    /// (first-wins); `None` while the surface is outstanding.
    pub action: Option<String>,
}

impl SurfaceSnapshot {
    /// Every published surface in the session log, in publication order,
    /// annotated with its first-wins resolution.
    pub(crate) fn from_entries(
        entries: &[(u64, harnx_core::session::SessionLogEntry)],
    ) -> anyhow::Result<Vec<Self>> {
        let mut out = Vec::new();
        for (seq, surface) in harnx_runtime::surfaces::published_surfaces(entries)? {
            let Some(surface_id) = harnx_runtime::surfaces::surface_id_of(&surface.payload) else {
                continue;
            };
            let action =
                harnx_runtime::surfaces::existing_surface_action(entries, &surface_id, seq)
                    .map(|accepted| accepted.action);
            out.push(Self {
                surface_id,
                publish_seq: seq,
                payload: surface.payload,
                action,
            });
        }
        Ok(out)
    }
}

/// The open action form for one surface. Reuses the T4.2 A2UI widgets.
#[derive(Clone)]
pub(crate) struct SurfaceActionModal {
    pub snapshot: SurfaceSnapshot,
    /// `None` when the payload has no actionable buttons: the form renders a
    /// notice and cannot submit.
    pub a2ui: Option<crate::interaction_confirmation::a2ui::A2UIFormState>,
    pub note: ratatui_textarea::TextArea<'static>,
    pub scroll: u16,
    pub last_key_at: std::time::Instant,
    pub submitting: bool,
    pub error: Option<String>,
}

impl SurfaceActionModal {
    pub(crate) fn new(snapshot: SurfaceSnapshot) -> Self {
        let a2ui = crate::interaction_confirmation::a2ui::A2UIFormState::new(&snapshot.payload);
        Self {
            a2ui,
            note: ratatui_textarea::TextArea::default(),
            snapshot,
            scroll: 0,
            last_key_at: std::time::Instant::now(),
            submitting: false,
            error: None,
        }
    }

    /// Display-only comparison: transient focus/scroll state is excluded, like
    /// `InteractionModal::same_request`.
    pub(crate) fn same_form(&self, other: &Self) -> bool {
        (
            &self.snapshot,
            &self.a2ui,
            self.submitting,
            &self.error,
            self.note_text().as_str(),
        ) == (
            &other.snapshot,
            &other.a2ui,
            other.submitting,
            &other.error,
            other.note_text().as_str(),
        )
    }

    fn note_text(&self) -> String {
        self.note.lines().join("\n")
    }
}

pub(crate) enum SurfaceEvent {
    Snapshot {
        generation: u64,
        surfaces: Result<Vec<SurfaceSnapshot>, String>,
    },
    Decided {
        generation: u64,
        surface_id: String,
        result: Result<SurfaceActionOutcome, String>,
    },
}

/// Durable surface state for the active session target.
#[derive(Default)]
pub(crate) struct SurfaceClient {
    target: Option<Target>,
    generation: u64,
    observer: Option<JoinHandle<()>>,
    snapshots: HashMap<String, SurfaceSnapshot>,
    read_error: Option<String>,
    decisions: Vec<JoinHandle<()>>,
}

impl Drop for SurfaceClient {
    fn drop(&mut self) {
        if let Some(task) = self.observer.take() {
            task.abort();
        }
        for task in self.decisions.drain(..) {
            task.abort();
        }
    }
}

impl SurfaceClient {
    fn check_target(&self) -> Result<Target, String> {
        if let Some(error) = &self.read_error {
            return Err(format!("Cannot refresh surfaces: {error}"));
        }
        self.target
            .clone()
            .ok_or_else(|| "Session is no longer selected".into())
    }
}

struct SurfaceTaskContext {
    config: harnx_runtime::config::GlobalConfig,
    local_worker: std::sync::Arc<
        tokio::sync::Mutex<Option<harnx_runtime::local_orchestrator::LocalWorkerSupervisor>>,
    >,
    event_tx: UnboundedSender<TuiEvent>,
    target: Target,
    generation: u64,
    submission: SurfaceActionSubmission,
}

fn spawn_surface_task(ctx: SurfaceTaskContext) -> JoinHandle<()> {
    tokio::spawn(async move {
        let result = async {
            let session = crate::remote_session::nats_session_for_target(
                &ctx.config,
                &ctx.local_worker,
                ctx.target.0.clone(),
                ctx.target.1.clone(),
            )
            .await?;
            session.submit_surface_action(&ctx.submission).await
        }
        .await
        .map_err(|error: anyhow::Error| format!("{error:#}"));
        let _ = ctx.event_tx.send(TuiEvent::Surface(SurfaceEvent::Decided {
            generation: ctx.generation,
            surface_id: ctx.submission.surface_id.clone(),
            result,
        }));
    })
}

// qual:allow(coupling, oi) reason: "Surface handling extends the shared Tui, following the per-concern module split of interaction_confirmation."
impl Tui {
    /// Keep the durable surface observer aligned with the active session, like
    /// `sync_interaction_client`. Never presents anything: surfaces are
    /// non-modal and the user opens the form explicitly.
    pub(super) fn sync_surface_client(&mut self) {
        let target = self.session_activity_destination();
        if self.surfaces.target != target {
            let generation = self.surfaces.generation.wrapping_add(1);
            let decisions = std::mem::take(&mut self.surfaces.decisions);
            self.surfaces = SurfaceClient {
                target: target.clone(),
                generation,
                decisions,
                observer: None,
                snapshots: HashMap::new(),
                read_error: None,
            };
            self.surfaces.observer = target.map(|target| {
                observe::spawn(
                    self.config.clone(),
                    self.event_tx.clone(),
                    target,
                    generation,
                )
            });
        }
    }

    pub(super) fn handle_surface_event(&mut self, event: SurfaceEvent) {
        match event {
            SurfaceEvent::Snapshot {
                generation,
                surfaces,
            } => {
                if generation != self.surfaces.generation {
                    return;
                }
                match surfaces {
                    Ok(snapshots) => {
                        self.surfaces.read_error = None;
                        self.surfaces.snapshots = snapshots
                            .into_iter()
                            .map(|snapshot| (snapshot.surface_id.clone(), snapshot))
                            .collect();
                    }
                    Err(error) => {
                        if self.surfaces.read_error.as_ref() != Some(&error) {
                            self.surface_notice(format!("Cannot refresh surfaces: {error}"));
                        }
                        self.surfaces.read_error = Some(error);
                    }
                }
                self.reconcile_surface_items();
            }
            SurfaceEvent::Decided {
                generation,
                surface_id,
                result,
            } => self.finish_surface_decision(generation, surface_id, result),
        }
    }

    /// Mirror the durable surface snapshot into the transcript: append rows
    /// for surfaces the transcript does not show yet (live path; the history
    /// rebuild places them next to their tool result) and refresh resolution
    /// everywhere — including surfaces answered from another frontend.
    fn reconcile_surface_items(&mut self) {
        for snapshot in self.surfaces.snapshots.values() {
            let already_shown = self
                .app
                .transcript
                .iter()
                .any(|item| matches!(item, TranscriptItem::UserSurface { surface_id, .. } if surface_id == &snapshot.surface_id));
            if already_shown {
                continue;
            }
            self.app.transcript.push(TranscriptItem::UserSurface {
                surface_id: snapshot.surface_id.clone(),
                payload: snapshot.payload.clone(),
                resolved: snapshot.action.clone(),
            });
        }
        for item in &mut self.app.transcript {
            if let TranscriptItem::UserSurface {
                surface_id,
                resolved,
                ..
            } = item
            {
                if let Some(snapshot) = self.surfaces.snapshots.get(surface_id) {
                    if resolved != &snapshot.action {
                        *resolved = snapshot.action.clone();
                    }
                }
            }
        }
    }

    /// Open the action form for a focused `UserSurface` row (Enter). Returns
    /// `true` when the focused row was a surface, including refusals, so the
    /// root-item dispatch stops like the handoff/sub-agent arms.
    pub(super) fn open_focused_root_surface(&mut self) -> bool {
        let Some(focus) = self.app.transcript_focus else {
            return false;
        };
        let Some(TranscriptItem::UserSurface {
            surface_id,
            resolved,
            ..
        }) = self.app.transcript.get(focus)
        else {
            return false;
        };
        // End the transcript borrow before the refusal guard mutates self.
        let (surface_id, resolved) = (surface_id.clone(), resolved.clone());
        if self.refuse_when_pending_interaction() {
            // Opening another surface while a confirmation is pending; the
            // pending tray is not a dismissible dialog (plan decision 6).
            return true;
        }
        if let Some(resolved) = resolved {
            self.app.show_footer_notice(&format!(
                "Surface already answered: {resolved}. Surfaces are first-wins."
            ));
            return true;
        }
        let Some(snapshot) = self.surfaces.snapshots.get(&surface_id).cloned() else {
            self.app.show_footer_notice(
                "Surface is not actionable yet (waiting for the durable record).",
            );
            return true;
        };
        if self.app.modal.is_some() {
            self.app.show_footer_notice("Close the open dialog first.");
            return true;
        }
        if snapshot.action.is_some() {
            self.app.show_footer_notice(&format!(
                "Surface already answered: {}. Surfaces are first-wins.",
                snapshot.action.as_deref().unwrap_or_default()
            ));
            return true;
        }
        self.app.modal = Some(ModalState::SurfaceAction(Box::new(
            SurfaceActionModal::new(snapshot),
        )));
        true
    }

    /// Submit the action the form selected, through the Part A client path.
    // qual:allow(dry, boilerplate) reason: "the submission and task context each own their cloned handle (session id, config, event channel); no shared constructor exists across frontends"
    pub(crate) fn submit_surface_action(&mut self, action: String) {
        let Some(ModalState::SurfaceAction(state)) = self.app.modal.as_mut() else {
            return;
        };
        if state.submitting {
            return;
        }
        let Some(form) = state.a2ui.as_ref() else {
            return;
        };
        let data = match harnx_core::interaction::response::build_data(
            &state.snapshot.payload,
            &action,
            &form.values(),
        ) {
            Ok(data) => data,
            Err(error) => {
                state.error = Some(error);
                return;
            }
        };
        let target = match self.surfaces.check_target() {
            Ok(target) => target,
            Err(error) => {
                state.error = Some(error);
                return;
            }
        };
        // The note is the user message that drives `note`-visibility text and
        // `turn`-visibility turns, so it maps to `user_message`, not the
        // response note.
        let user_message = {
            let note = state.note_text();
            (!note.trim().is_empty()).then_some(note)
        };
        let submission = SurfaceActionSubmission {
            surface_id: state.snapshot.surface_id.clone(),
            publish_seq: state.snapshot.publish_seq,
            response: InteractionResponse {
                action,
                data,
                note: None,
            },
            user_message,
        };
        state.submitting = true;
        state.error = None;
        let ctx = SurfaceTaskContext {
            config: self.config.clone(),
            local_worker: self.local_worker.clone(),
            event_tx: self.event_tx.clone(),
            target,
            generation: self.surfaces.generation,
            submission,
        };
        self.surfaces.decisions.retain(|task| !task.is_finished());
        self.surfaces.decisions.push(spawn_surface_task(ctx));
    }

    fn finish_surface_decision(
        &mut self,
        generation: u64,
        surface_id: String,
        result: Result<SurfaceActionOutcome, String>,
    ) {
        let error = surface_decision_error(&result);
        if let Some(error) = &error {
            self.surface_notice(format!("Surface '{surface_id}' action rejected: {error}"));
        }
        if generation != self.surfaces.generation {
            return;
        }
        self.surfaces.decisions.retain(|task| !task.is_finished());
        if error.is_none() && result.is_ok() {
            // Optimistically mark the row; the observer's next snapshot
            // confirms the durable resolution.
            mark_surface_answered(&mut self.app.transcript, &surface_id);
        }
        if let Some(ModalState::SurfaceAction(state)) = self.app.modal.as_mut() {
            if state.snapshot.surface_id == surface_id && state.submitting {
                state.submitting = false;
                if error.is_none() {
                    self.app.modal = None;
                } else {
                    state.error = error.clone();
                }
            }
        }
    }

    fn surface_notice(&mut self, message: String) {
        self.app.show_footer_notice(&message);
    }
}

/// Optimistically mark one surface row as answered, pending the observer's
/// confirming snapshot.
fn mark_surface_answered(transcript: &mut [TranscriptItem], surface_id: &str) {
    for item in transcript {
        if let TranscriptItem::UserSurface {
            surface_id: id,
            resolved,
            ..
        } = item
        {
            if id == surface_id && resolved.is_none() {
                *resolved = Some("answered".into());
            }
        }
    }
}

fn surface_decision_error(result: &Result<SurfaceActionOutcome, String>) -> Option<String> {
    match result {
        Ok(SurfaceActionOutcome::Applied | SurfaceActionOutcome::AlreadyApplied) => None,
        Ok(SurfaceActionOutcome::Conflicting) => {
            Some("conflicting: this surface was already answered differently".into())
        }
        Ok(SurfaceActionOutcome::UnknownSurface) => {
            Some("unknown_surface: no published surface matches".into())
        }
        Ok(SurfaceActionOutcome::UndeclaredAction) => {
            Some("undeclared_action: not a declared button on this surface".into())
        }
        Ok(SurfaceActionOutcome::InvalidVisibility) => {
            Some("invalid_visibility: the button declares an unknown visibility".into())
        }
        Ok(SurfaceActionOutcome::InvalidData) => {
            Some("invalid_data: data does not match the surface's inputs".into())
        }
        Ok(SurfaceActionOutcome::InvalidTool) => {
            Some("invalid_tool: the declared target tool is not valid".into())
        }
        Ok(SurfaceActionOutcome::UnauthorizedTool) => Some(
            "unauthorized_tool: the declared target tool is not available in this session".into(),
        ),
        Ok(SurfaceActionOutcome::ToolFailed) => {
            Some("tool_failed: the declared target tool failed".into())
        }
        Ok(SurfaceActionOutcome::LeaseLost) => Some(
            "lease_lost: the action outcome is uncertain; a direct tool may have run. \
             Check the session before retrying."
                .into(),
        ),
        Ok(SurfaceActionOutcome::AppendFailed) => Some(
            "append_failed: a handler panic or record failure left the action outcome \
             uncertain; a direct tool may have run. Check the session before retrying."
                .into(),
        ),
        Ok(SurfaceActionOutcome::Pending) => Some(
            "pending: the action is still being processed; check its outcome before retrying."
                .into(),
        ),
        Err(error) => Some(error.clone()),
    }
}

/// One-line headline for a surface payload: the first text component, else the
/// surface id. Used for the transcript row; the full surface renders in the
/// form modal.
pub(crate) fn surface_headline(payload: &serde_json::Value) -> String {
    let components = harnx_core::interaction::response::components(payload);
    for component in components {
        if component
            .get("component")
            .and_then(serde_json::Value::as_str)
            == Some("Text")
        {
            if let Some(text) = component.get("text").and_then(serde_json::Value::as_str) {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    return trimmed.to_string();
                }
            }
        }
    }
    harnx_runtime::surfaces::surface_id_of(payload).unwrap_or_else(|| "surface".into())
}
