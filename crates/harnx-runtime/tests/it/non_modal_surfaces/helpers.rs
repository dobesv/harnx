//! Shared assertions and submission helpers for the non-modal surface tests.

use crate::worker::CI_SAFE_TIMEOUT;
use anyhow::Result;
use harnx_core::message::MessageRole;
use harnx_core::session::SessionLogEntry;
use harnx_runtime::nats_session::NatsSession;
use harnx_runtime::surfaces::{SurfaceActionOutcome, SurfaceActionSubmission};
use serde_json::Value;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use super::fixture::SurfaceFixture;
use super::stub::reset_model_state;

/// A submission with empty data and an optional operator message, the shape
/// every note/turn-style test action uses.
pub(super) fn action_submission(
    surface_id: impl Into<String>,
    publish_seq: u64,
    action: &str,
    user_message: Option<&str>,
) -> SurfaceActionSubmission {
    SurfaceActionSubmission {
        surface_id: surface_id.into(),
        publish_seq,
        response: harnx_core::interaction::InteractionResponse {
            action: action.to_string(),
            data: serde_json::Map::new(),
            note: None,
        },
        user_message: user_message.map(str::to_string),
    }
}

pub(super) fn first_published_surface(entries: &[(u64, SessionLogEntry)]) -> Option<(u64, String)> {
    entries.iter().find_map(|(seq, entry)| {
        let SessionLogEntry::ToolResults { results, .. } = entry else {
            return None;
        };
        results.iter().find_map(|output| {
            output
                .user_surfaces
                .iter()
                .find_map(|payload| {
                    payload
                        .pointer("/createSurface/surfaceId")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .map(|surface_id| (*seq, surface_id))
        })
    })
}

pub(super) fn wait_for_turn_end_after(
    fixture: &SurfaceFixture,
    after_seq: u64,
) -> impl std::future::Future<Output = Result<()>> + '_ {
    use std::future::IntoFuture;
    async move {
        let deadline = std::time::Instant::now() + CI_SAFE_TIMEOUT;
        while std::time::Instant::now() < deadline {
            let entries = fixture.entries().await?;
            let done = entries.iter().any(|(seq, entry)| {
                *seq > after_seq && matches!(entry, SessionLogEntry::TurnEnd { .. })
            });
            if done {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        anyhow::bail!("turn never completed after seq {after_seq}")
    }
    .into_future()
}

/// Enqueue a user prompt with external admission and return the durable
/// tail before it: turn-end waits key on that sequence.
pub(super) async fn enqueue_prompt(fixture: &SurfaceFixture, prompt: &str) -> Result<u64> {
    let entries = fixture.entries().await?;
    let before = entries.last().map_or(0, |(seq, _)| *seq);
    fixture
        .session
        .clone()
        .with_external_admission()
        .enqueue_text(prompt)
        .await?
        .into_activation_result()?;
    Ok(before)
}

/// Seed a prompt and wait for the turn to complete.
pub(super) async fn run_single_turn(fixture: &SurfaceFixture, prompt: &str) -> Result<u64> {
    let before = enqueue_prompt(fixture, prompt).await?;
    wait_for_turn_end_after(fixture, before).await?;
    Ok(before)
}

/// Start a turn and return immediately, so the worker holds the session
/// lease and answers control commands while the test acts on a surface.
pub(super) async fn start_running_turn(fixture: &SurfaceFixture, prompt: &str) -> Result<u64> {
    enqueue_prompt(fixture, prompt).await
}

/// Start the fixture, run its seeding turn to completion, and return the
/// first published surface: the idle-path tests act after the lease is
/// released.
pub(super) async fn single_surface() -> Result<(SurfaceFixture, String, u64)> {
    reset_model_state();
    let fixture = SurfaceFixture::start("single").await?;
    run_single_turn(&fixture, "prepare the deployment").await?;
    let (surface_id, publish_seq) = fixture.wait_surface_published().await?;
    Ok((fixture, surface_id, publish_seq))
}

/// Start the fixture's gated turn and return the first published surface:
/// every at-most-once test races a slow direct tool against a live worker
/// that holds the lease.
pub(super) async fn gated_surface() -> Result<(SurfaceFixture, String, u64)> {
    reset_model_state();
    let fixture = SurfaceFixture::start("gated").await?;
    let _before = start_running_turn(&fixture, "prepare the deployment").await?;
    let (surface_id, publish_seq) = fixture.wait_surface_published().await?;
    Ok((fixture, surface_id, publish_seq))
}

pub(super) fn user_message_texts(entries: &[(u64, SessionLogEntry)]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|(_, entry)| {
            let SessionLogEntry::Message {
                role, content, id, ..
            } = entry
            else {
                return None;
            };
            // Runtime-authored rows (hook context, synthesized notes, the
            // deadline timing note) are context, not user prompts.
            let runtime_authored = harnx_core::runtime_note::is_runtime_context_id(id.as_deref())
                || id.as_deref().is_some_and(|id| {
                    id.starts_with(harnx_core::timing_identity::TIMING_MESSAGE_ID_PREFIX)
                });
            (role == &MessageRole::User && !runtime_authored).then(|| content.to_text())
        })
        .collect()
}

pub(super) fn surface_action_entries(entries: &[(u64, SessionLogEntry)]) -> Vec<&SessionLogEntry> {
    entries
        .iter()
        .filter_map(|(_, entry)| match entry {
            SessionLogEntry::SurfaceAction { .. } => Some(entry),
            _ => None,
        })
        .collect()
}
/// Wait until the `slow` direct tool target has started executing.
pub(super) async fn wait_slow_started(fixture: &SurfaceFixture) -> Result<()> {
    let deadline = std::time::Instant::now() + CI_SAFE_TIMEOUT;
    while fixture.slow_counter.load(Ordering::SeqCst) == 0 {
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "slow tool never started executing"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Ok(())
}

/// Hold the parked slow-tool window until the client's whole recovery
/// budget has elapsed since `submitted_at` (the budget is the fixture's
/// [`RECOVERY_BUDGET`] knob). An acknowledged client must neither resend
/// nor fall back to its own idle execution — which would re-run the target
/// — while the worker owns the attempt, and only an elapsed budget can
/// prove that negative, so this wait is temporal by necessity. The counter
/// poll fails fast on the at-most-once violation the window exists to
/// catch instead of failing later on the final counts.
pub(super) async fn hold_parked_past_recovery_budget(
    fixture: &SurfaceFixture,
    submitted_at: std::time::Instant,
) -> Result<()> {
    // Scheduling slack so "elapsed" is certain, not marginal.
    const BUDGET_SLACK: Duration = Duration::from_millis(200);
    let deadline = submitted_at + super::fixture::RECOVERY_BUDGET + BUDGET_SLACK;
    while std::time::Instant::now() < deadline {
        anyhow::ensure!(
            fixture.slow_counter.load(Ordering::SeqCst) == 1,
            "the parked window re-ran the target tool"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Ok(())
}

/// Count the `SurfaceAction` commands published on the session's control
/// subject: the concurrent at-most-once tests use this as their dispatch
/// barrier — waiting until every submission's command is on the wire
/// proves the race is live before the parked window ends, where a fixed
/// sleep only guessed at it.
pub(super) async fn subscribe_control_commands(
    fixture: &SurfaceFixture,
) -> Result<Arc<std::sync::atomic::AtomicUsize>> {
    let subject = harnx_runtime::nats_worker::control_subject(fixture.session.storage_key());
    let mut commands = fixture.client.subscribe(subject).await?;
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = count.clone();
    tokio::spawn(async move {
        use futures_util::StreamExt;
        while let Some(message) = commands.next().await {
            if matches!(
                harnx_runtime::nats_worker::ControlCommand::from_bytes(&message.payload),
                Ok(harnx_runtime::nats_worker::ControlCommand::SurfaceAction { .. })
            ) {
                counter.fetch_add(1, Ordering::SeqCst);
            }
        }
    });
    // The subscription must be live before the submissions spawn, or an
    // early command could slip past the count.
    fixture.client.flush().await?;
    Ok(count)
}

/// Wait until the control-subject barrier has observed `expected`
/// `SurfaceAction` commands, bounded by [`CI_SAFE_TIMEOUT`].
pub(super) async fn wait_control_commands(
    commands: &Arc<std::sync::atomic::AtomicUsize>,
    expected: usize,
) -> Result<()> {
    let deadline = std::time::Instant::now() + CI_SAFE_TIMEOUT;
    while commands.load(Ordering::SeqCst) < expected {
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "only {} of {expected} surface action commands reached the control subject",
            commands.load(Ordering::SeqCst)
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Ok(())
}

/// Append an unrelated durable row while a surface action's tool call is in
/// flight, so its record append loses the CAS race against a moved tail.
/// The running turn appends constantly, so this races for its own tail too.
pub(super) async fn append_unrelated_row(fixture: &SurfaceFixture, text: &str) -> Result<()> {
    let entry = SessionLogEntry::Message {
        id: None,
        role: MessageRole::Assistant,
        content: harnx_core::message::MessageContent::Text(text.into()),
        timestamp: None,
        fence_token: None,
    };
    for _ in 0..50 {
        let entries = fixture.entries().await?;
        let expected = entries.last().map_or(0, |(seq, _)| *seq);
        match fixture
            .log
            .append_fenced(&entry, expected, &uuid::Uuid::new_v4().to_string())
            .await?
        {
            harnx_runtime::nats_session_log::FencedAppend::Appended(_) => return Ok(()),
            harnx_runtime::nats_session_log::FencedAppend::Conflict { .. } => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
    anyhow::bail!("unrelated append lost every CAS race against the running turn");
}

/// The `(surface_id, publish_seq)` of a durable published surface by id.
pub(super) async fn wait_surface_generation(
    fixture: &SurfaceFixture,
    surface_id: &str,
) -> Result<(String, u64)> {
    let deadline = std::time::Instant::now() + CI_SAFE_TIMEOUT;
    loop {
        let entries = fixture.entries().await?;
        let found = entries.iter().find_map(|(seq, entry)| {
            let SessionLogEntry::ToolResults { results, .. } = entry else {
                return None;
            };
            results.iter().find_map(|output| {
                output
                    .user_surfaces
                    .iter()
                    .find(|payload| {
                        payload
                            .pointer("/createSurface/surfaceId")
                            .and_then(Value::as_str)
                            == Some(surface_id)
                    })
                    .map(|_| (*seq, surface_id.to_string()))
            })
        });
        if let Some((seq, id)) = found {
            return Ok((id, seq));
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "surface '{surface_id}' never published durably"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The durable `SurfaceAction` records, deconstructed for assertions.
pub(super) fn recorded_actions(
    entries: &[(u64, SessionLogEntry)],
) -> Vec<(String, Option<String>)> {
    entries
        .iter()
        .filter_map(|(_, entry)| {
            let SessionLogEntry::SurfaceAction {
                action,
                visibility,
                note_text,
                ..
            } = entry
            else {
                return None;
            };
            Some((format!("{action}/{visibility}"), note_text.clone()))
        })
        .collect()
}

/// A background surface action submission still awaiting its outcome.
pub(super) type SubmitHandle = tokio::task::JoinHandle<Result<SurfaceActionOutcome>>;

/// Submit the slow direct tool and wait until its single execution parks:
/// the parked window is where each at-most-once scenario differs.
pub(super) async fn start_parked_slow_action(
    fixture: &SurfaceFixture,
    surface_id: &str,
    publish_seq: u64,
) -> Result<SubmitHandle> {
    let submit = spawn_submission(
        fixture.session.clone(),
        action_submission(surface_id, publish_seq, "slow_act", None),
    );
    wait_slow_started(fixture).await?;
    Ok(submit)
}

/// Background submission of an action on the session: the at-most-once
/// tests park a slow target while this runs, then assert on the outcome.
pub(super) fn spawn_submission(
    session: NatsSession,
    submission: SurfaceActionSubmission,
) -> SubmitHandle {
    tokio::spawn(async move { session.submit_surface_action(&submission).await })
}
