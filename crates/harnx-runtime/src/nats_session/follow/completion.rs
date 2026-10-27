//! Durable update handling during follower polling.
use super::{FollowLoopContext, FollowResult, FollowStreamDriver};
use crate::nats_session::NatsSession;
use anyhow::Result;
use harnx_core::session::SessionLogEntry;

pub(super) struct CompletionDurableEffect {
    pub observed_boundary: Option<FollowResult>,
    pub final_response: Option<String>,
    pub turn_error: Option<String>,
    pub turn_complete: bool,
    pub completion_error: Option<anyhow::Error>,
}

struct CompletionCandidate<'a> {
    pub entries: &'a [(u64, SessionLogEntry)],
    pub orphaned: Option<String>,
    pub completion_visible: bool,
    pub observed_boundary: Option<FollowResult>,
}

impl NatsSession {
    pub(super) async fn handle_completion_durable_update(
        &self,
        update: crate::nats_session::completion::DurableUpdate,
        cx: &FollowLoopContext<'_>,
        driver: &mut FollowStreamDriver<'_>,
    ) -> Result<CompletionDurableEffect> {
        let entries = update.entries;
        let orphaned = update.orphaned;
        driver.stream.refresh_generation().await?;
        let (observed_boundary, completion_visible) =
            self.evaluate_completion_visibility(&entries, cx)?;
        Self::sync_effective_log_advisories(&entries, cx, driver);
        Self::resolve_completion_effect(
            CompletionCandidate {
                entries: &entries,
                orphaned,
                completion_visible,
                observed_boundary,
            },
            cx,
        )
    }

    fn evaluate_completion_visibility(
        &self,
        entries: &[(u64, SessionLogEntry)],
        cx: &FollowLoopContext<'_>,
    ) -> Result<(Option<FollowResult>, bool)> {
        let observed_boundary = match cx.boundary_options {
            Some(boundary_options) => {
                if cx.target.invocation_id.is_empty() {
                    anyhow::bail!("boundary follow has no invocation identity");
                }
                self.observe_follow_boundary(entries, cx.target, boundary_options)?
            }
            None => None,
        };
        let effective = harnx_core::session_reconstruct::apply_log_mutations_nats(entries).ok();
        let completion_visible = if cx.boundary_options.is_some() {
            observed_boundary.is_some()
        } else {
            Self::is_turn_completion_visible(entries, effective.as_deref(), cx.target.prompt_seq)
        };
        Ok((observed_boundary, completion_visible))
    }

    fn sync_effective_log_advisories(
        entries: &[(u64, SessionLogEntry)],
        cx: &FollowLoopContext<'_>,
        driver: &mut FollowStreamDriver<'_>,
    ) {
        if let Ok(effective) = harnx_core::session_reconstruct::apply_log_mutations_nats(entries) {
            driver.state.cached_effective = Some(effective);
            crate::nats_session::emit_all_logical_seqs_for_window(
                driver.state.cached_effective.as_deref(),
                cx.event_sink,
                &mut driver.state.emitted_logical_seqs,
            );
            crate::nats_session::flush_pending_advisories(
                &mut driver.state.pending_advisories,
                driver.state.cached_effective.as_deref(),
                cx.event_sink,
                &mut driver.state.emitted_logical_seqs,
                crate::nats_session::AdvisoryFlush::Live,
                cx.live,
                driver.stream.last_applied_seq(),
            );
        }
        crate::nats_session::completion::reconcile_subagent_progress(
            driver.state.cached_effective.as_deref().unwrap_or(entries),
            cx.target.prompt_seq,
            cx.event_sink,
            &mut driver.state.emitted_subagent_completions,
        );
    }

    fn resolve_completion_effect(
        mut candidate: CompletionCandidate<'_>,
        cx: &FollowLoopContext<'_>,
    ) -> Result<CompletionDurableEffect> {
        if candidate.completion_visible {
            let (final_response, turn_error) =
                Self::extract_turn_outcome(candidate.entries, cx.target.prompt_seq);
            return Ok(CompletionDurableEffect {
                observed_boundary: candidate.observed_boundary,
                final_response,
                turn_error,
                turn_complete: true,
                completion_error: None,
            });
        }
        if let Some(reason) = candidate.orphaned.take() {
            return Self::resolve_orphaned_completion(candidate, reason, cx);
        }
        Ok(CompletionDurableEffect {
            observed_boundary: candidate.observed_boundary,
            final_response: None,
            turn_error: None,
            turn_complete: false,
            completion_error: None,
        })
    }

    fn resolve_orphaned_completion(
        candidate: CompletionCandidate<'_>,
        reason: String,
        cx: &FollowLoopContext<'_>,
    ) -> Result<CompletionDurableEffect> {
        if cx.boundary_options.is_some() {
            return Ok(CompletionDurableEffect {
                observed_boundary: candidate.observed_boundary,
                final_response: None,
                turn_error: None,
                turn_complete: false,
                completion_error: Some(anyhow::anyhow!(
                    "Cannot confirm durable follow boundary: {reason}"
                )),
            });
        }
        let (final_response, mut turn_error) =
            Self::extract_turn_outcome(candidate.entries, cx.target.prompt_seq);
        if final_response.is_none() {
            turn_error = Some(reason);
        }
        Ok(CompletionDurableEffect {
            observed_boundary: candidate.observed_boundary,
            final_response,
            turn_error,
            turn_complete: true,
            completion_error: None,
        })
    }
}
