use crate::cli::{Cli, CompactArgs, CompactCommands, CompactSessionArgs};
use crate::config::WorkingMode;
use crate::init_frontend_config;
use anyhow::Result;
use harnx_core::event::{AgentEvent, SessionEvent};
use harnx_core::session::{CompactOutcome, SessionLogEntry, UnchangedReason};

pub(crate) async fn run_compact_command(compact_args: &CompactArgs, _cli: &Cli) -> Result<u8> {
    match &compact_args.command {
        CompactCommands::Session(args) => run_compact_session(args).await,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum FollowAction {
    Continue,
    Done(u8),
}

fn process_compact_outcome(outcome: &CompactOutcome) -> FollowAction {
    match outcome {
        CompactOutcome::Compacted => println!("Session compacted successfully"),
        CompactOutcome::Unchanged(reason) => {
            let reason = match reason {
                UnchangedReason::NoUserMessages => "no user messages",
                UnchangedReason::NothingEligible => "nothing eligible",
                UnchangedReason::AlreadyCompacted => "already compacted",
            };
            println!("Compaction completed: nothing to compact ({reason})");
        }
        CompactOutcome::Failed(message) => {
            eprintln!("Compaction failed: {message}");
            return FollowAction::Done(1);
        }
    }
    FollowAction::Done(0)
}

fn process_compact_event(event: &SessionEvent, target_id: &str) -> FollowAction {
    match event {
        SessionEvent::CompactingStarted { compaction_id }
            if compaction_id.as_deref() == Some(target_id) =>
        {
            println!("Compacting session...");
            FollowAction::Continue
        }
        SessionEvent::CompactingCompleted {
            compaction_id,
            outcome,
        } if compaction_id.as_deref() == Some(target_id) => process_compact_outcome(outcome),
        SessionEvent::CompactingFailed {
            compaction_id,
            error,
        } if compaction_id.as_deref() == Some(target_id) => {
            eprintln!("Compaction failed: {error}");
            FollowAction::Done(1)
        }
        _ => FollowAction::Continue,
    }
}

fn process_compact_entries(entries: &[(u64, SessionLogEntry)], target_id: &str) -> FollowAction {
    for (_, entry) in entries {
        match entry {
            SessionLogEntry::CompactResult {
                compaction_id,
                outcome,
                ..
            } if compaction_id == target_id => return process_compact_outcome(outcome),
            _ => {}
        }
    }
    FollowAction::Continue
}

async fn refresh_compact_result(
    stream: &mut harnx_runtime::nats_event_sink::SessionEventStream,
    target_id: &str,
) -> FollowAction {
    let old_len = stream.history().len();
    match stream.refresh_history().await {
        Ok(true) => process_compact_entries(&stream.history()[old_len..], target_id),
        Ok(false) => FollowAction::Continue,
        Err(error) => {
            log::warn!("failed to refresh durable compaction result: {error:#}");
            FollowAction::Continue
        }
    }
}

async fn process_compact_advisory(
    maybe_event: Option<harnx_runtime::nats_event_sink::AdvisoryEnvelope>,
    stream: &mut harnx_runtime::nats_event_sink::SessionEventStream,
    target_id: &str,
) -> Result<FollowAction> {
    let Some(envelope) = maybe_event else {
        return match refresh_compact_result(stream, target_id).await {
            FollowAction::Continue => {
                eprintln!("Event stream closed before compaction completed");
                Err(anyhow::anyhow!("event stream closed"))
            }
            action => Ok(action),
        };
    };
    Ok(match envelope.event {
        AgentEvent::Session(event) => process_compact_event(&event, target_id),
        _ => FollowAction::Continue,
    })
}

async fn process_compact_timeout(
    stream: &mut harnx_runtime::nats_event_sink::SessionEventStream,
    target_id: &str,
) -> FollowAction {
    match refresh_compact_result(stream, target_id).await {
        FollowAction::Continue => {
            println!("Timeout waiting for compaction (request id: {target_id}). Exiting wait.");
            FollowAction::Done(2)
        }
        action => action,
    }
}

async fn follow_compaction(
    stream: &mut harnx_runtime::nats_event_sink::SessionEventStream,
    target_id: &str,
    timeout_secs: u64,
) -> Result<u8> {
    let timeout_deadline = (timeout_secs > 0)
        .then(|| tokio::time::Instant::now() + tokio::time::Duration::from_secs(timeout_secs));
    let timeout = async move {
        match timeout_deadline {
            Some(deadline) => tokio::time::sleep_until(deadline).await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(timeout);

    let mut durable_poll = tokio::time::interval(tokio::time::Duration::from_millis(1000));
    durable_poll.tick().await;

    loop {
        let action = tokio::select! {
            maybe_event = stream.next() => {
                process_compact_advisory(maybe_event, stream, target_id).await?
            }
            _ = durable_poll.tick() => refresh_compact_result(stream, target_id).await,
            _ = &mut timeout => process_compact_timeout(stream, target_id).await,
            _ = tokio::signal::ctrl_c() => {
                println!("Interrupted. Compaction still pending (request id: {target_id}).");
                FollowAction::Done(130)
            }
        };
        if let FollowAction::Done(exit_code) = action {
            return Ok(exit_code);
        }
    }
}

async fn run_compact_session(args: &CompactSessionArgs) -> Result<u8> {
    use harnx_runtime::nats_session::{
        request_compaction_session, CompactSubmit, CompactionRequest,
    };
    use harnx_runtime::nats_worker::SessionActivationRoute;

    let config = init_frontend_config(WorkingMode::Cmd, true).await?;
    let (_, cluster) = config.resolve_session_agent(&args.agent)?;
    let (jetstream, metadata) =
        harnx_runtime::config::session_metadata_for_agent(&config, &args.agent, &args.session)
            .await?;
    let client = config.nats_client(&cluster).await?;

    // Attach stream BEFORE submitting request (subscribe-before-submit closes race)
    let mut stream = harnx_runtime::nats_event_sink::SessionEventStream::attach(
        jetstream.clone(),
        client.clone(),
        &metadata.storage_key(),
    )
    .await?;

    // Build compaction request
    let compaction_id = uuid::Uuid::new_v4().to_string();
    let request = CompactionRequest {
        session_id: metadata.storage_key(),
        cluster: cluster.clone(),
        replicas: 1,
        compaction_id: compaction_id.clone(),
        requested_by: None,
    };

    // Submit compaction request directly
    let submit_result = request_compaction_session(
        &jetstream,
        &client,
        &SessionActivationRoute::ClusterShared,
        request,
    )
    .await?;

    // Determine target compaction_id and initial message
    let target_id: String = match submit_result {
        CompactSubmit::Submitted { ref compaction_id } => {
            println!("Compaction request submitted (id: {compaction_id})");
            compaction_id.clone()
        }
        CompactSubmit::AlreadyInFlight { ref compaction_id } => {
            println!(
                "Compaction already in progress (id: {compaction_id}), waiting for completion..."
            );
            compaction_id.clone()
        }
        CompactSubmit::NothingToDo { ref outcome } => {
            // Nothing to compact — report and exit 0
            match outcome {
                harnx_core::session::CompactOutcome::Compacted => {
                    println!("Session already compacted");
                }
                harnx_core::session::CompactOutcome::Unchanged(reason) => {
                    let reason_str = match reason {
                        UnchangedReason::NoUserMessages => "no user messages",
                        UnchangedReason::NothingEligible => "nothing eligible",
                        UnchangedReason::AlreadyCompacted => "already compacted",
                    };
                    println!("Nothing to compact ({reason_str})");
                }
                harnx_core::session::CompactOutcome::Failed(_) => unreachable!(),
            }
            return Ok(0);
        }
    };

    follow_compaction(&mut stream, &target_id, args.timeout).await
}

#[cfg(test)]
mod compact_follow_tests {
    use super::*;

    #[test]
    fn advisory_results_are_filtered_by_compaction_id() {
        let target_id = "manual-compaction-123";
        let automatic = SessionEvent::CompactingCompleted {
            compaction_id: None,
            outcome: CompactOutcome::Compacted,
        };
        let unrelated = SessionEvent::CompactingCompleted {
            compaction_id: Some("other-id".to_string()),
            outcome: CompactOutcome::Compacted,
        };
        let matching = SessionEvent::CompactingCompleted {
            compaction_id: Some(target_id.to_string()),
            outcome: CompactOutcome::Compacted,
        };

        assert_eq!(
            process_compact_event(&automatic, target_id),
            FollowAction::Continue
        );
        assert_eq!(
            process_compact_event(&unrelated, target_id),
            FollowAction::Continue
        );
        assert_eq!(
            process_compact_event(&matching, target_id),
            FollowAction::Done(0)
        );
    }

    #[test]
    fn durable_results_are_filtered_by_compaction_id() {
        let target_id = "manual-compaction-123";
        let unrelated = vec![(
            1,
            SessionLogEntry::compact_result("other-id", CompactOutcome::Compacted),
        )];
        let matching = vec![
            (
                1,
                SessionLogEntry::compact_result("other-id", CompactOutcome::Compacted),
            ),
            (
                2,
                SessionLogEntry::compact_result(
                    target_id,
                    CompactOutcome::Unchanged(UnchangedReason::NothingEligible),
                ),
            ),
        ];

        assert_eq!(
            process_compact_entries(&unrelated, target_id),
            FollowAction::Continue
        );
        assert_eq!(
            process_compact_entries(&matching, target_id),
            FollowAction::Done(0)
        );
    }

    #[test]
    fn failed_outcomes_return_failure_status() {
        assert_eq!(
            process_compact_outcome(&CompactOutcome::Failed("error message".to_string())),
            FollowAction::Done(1)
        );
    }
}
