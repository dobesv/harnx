//! Read-side crash and orphan recovery classification for tool rounds.
//!
//! Evaluates open round states across session logs, round-start ownership proofs,
//! private checkpoint store records, and exact invocation journal entries before
//! any synthetic completion or continuation write occurs.

#[cfg(test)]
#[path = "recovery_tests.rs"]
mod tests;

use super::{
    completion::public_output,
    records,
    snapshot::{CallCheckpoint, ResultSnapshot},
    store::{CheckpointStore, ReadCheckpoint},
};
use crate::{config::GlobalConfig, nats_worker::NatsSessionLogBackend};
use anyhow::{Context, Result};
use harnx_core::{
    session::SessionLogEntry,
    tool::{ToolCall, ToolResult},
};

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CallRecovery {
    Completed(Box<ToolResult>),
    Resume,
    Legacy,
    Interrupted(String),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RoundRecovery {
    pub owned: bool,
    pub started: bool,
    pub calls: Vec<CallRecovery>,
}

struct RecoveryContext<'a> {
    chain: &'a harnx_core::middleware::ResolvedMiddlewareChain,
    current_chain_version: &'a str,
    backend: &'a NatsSessionLogBackend,
    round: u64,
    effective: &'a [(u64, SessionLogEntry)],
    round_start_exists: bool,
    store: Option<&'a CheckpointStore>,
}

struct RequestBoundary<'a> {
    chain_version: &'a str,
    checkpoint: &'a str,
}

pub(crate) async fn classify(
    config: &GlobalConfig,
    backend: &NatsSessionLogBackend,
    round: u64,
    calls: &[ToolCall],
) -> Result<RoundRecovery> {
    let chain = config.read().resolved_middleware()?;
    let current_chain_version = chain.version.clone();

    // 1. Check round-start upfront via existing round_started
    let round_start_exists =
        super::round_started(&backend.jetstream(), backend.session_id(), round).await?;

    // 2. Load latest session log events and apply log mutations.
    let raw_entries = backend.load_events_latest_async().await?;
    let effective = harnx_core::session_reconstruct::apply_log_mutations_nats(&raw_entries)?;

    // Per-call interaction records determine ownership; SuspensionReady alone does not.
    let has_call_log_records = effective.iter().any(|(_, entry)| {
        matches!(
            entry,
            SessionLogEntry::ToolCallCompleted { tool_round_seq, .. }
                | SessionLogEntry::InteractionRequested { tool_round_seq, .. }
                if *tool_round_seq == round
        )
    });

    // 3. Open checkpoint store without creating it.
    let store_opt = CheckpointStore::open_existing(&backend.jetstream()).await?;

    let ctx = RecoveryContext {
        chain: &chain,
        current_chain_version: &current_chain_version,
        backend,
        round,
        effective: &effective,
        round_start_exists,
        store: store_opt.as_ref(),
    };

    let mut call_recoveries = Vec::with_capacity(calls.len());
    let mut has_any_attempts = false;

    for call in calls {
        let recovery = classify_call(&ctx, call, &mut has_any_attempts).await?;
        call_recoveries.push(recovery);
    }

    let owned = round_start_exists || has_call_log_records || has_any_attempts;

    Ok(RoundRecovery {
        owned,
        started: round_start_exists,
        calls: call_recoveries,
    })
}

fn find_latest_completed<'a>(
    effective: &'a [(u64, SessionLogEntry)],
    round: u64,
    call_id: Option<&str>,
) -> Option<(&'a harnx_core::session::ToolOutput, &'a str)> {
    effective.iter().rev().find_map(|(_, entry)| match entry {
        SessionLogEntry::ToolCallCompleted {
            tool_round_seq,
            tool_call_id,
            result: public,
            private_snapshot,
            ..
        } if *tool_round_seq == round && Some(tool_call_id.as_str()) == call_id => {
            Some((public, private_snapshot.as_str()))
        }
        _ => None,
    })
}

fn find_latest_request<'a>(
    effective: &'a [(u64, SessionLogEntry)],
    round: u64,
    call_id: Option<&str>,
) -> Option<RequestBoundary<'a>> {
    effective.iter().rev().find_map(|(_, entry)| match entry {
        SessionLogEntry::InteractionRequested {
            tool_round_seq,
            tool_call_id,
            chain_version,
            checkpoint,
            ..
        } if *tool_round_seq == round && Some(tool_call_id.as_str()) == call_id => {
            Some(RequestBoundary {
                chain_version: chain_version.as_str(),
                checkpoint: checkpoint.as_str(),
            })
        }
        _ => None,
    })
}

async fn read_call_attempts(
    ctx: &RecoveryContext<'_>,
    call_id: &str,
) -> Result<Result<Vec<records::AttemptStart>, String>> {
    let Some(store) = ctx.store else {
        return Ok(Ok(Vec::new()));
    };
    store
        .read_ordered_attempts(ctx.backend.session_id(), ctx.round, call_id)
        .await
}

async fn classify_call(
    ctx: &RecoveryContext<'_>,
    call: &ToolCall,
    has_any_attempts: &mut bool,
) -> Result<CallRecovery> {
    let call_id = call.id.as_deref().unwrap_or("");
    let call_prefix = records::call_prefix(ctx.backend.session_id(), ctx.round, call_id);

    if let Some((public, snapshot)) =
        find_latest_completed(ctx.effective, ctx.round, call.id.as_deref())
    {
        return restore_completed(ctx, call, &call_prefix, public, snapshot).await;
    }

    let attempts = match read_call_attempts(ctx, call_id).await? {
        Ok(atts) => atts,
        Err(missing) => {
            return Ok(CallRecovery::Interrupted(format!(
                "interrupted, outcome unknown: {missing}"
            )));
        }
    };
    if !attempts.is_empty() {
        *has_any_attempts = true;
    }

    let latest_attempt = attempts.last();
    let latest_request = find_latest_request(ctx.effective, ctx.round, call.id.as_deref());

    if let Some(req) = latest_request {
        classify_with_request(ctx, call, &call_prefix, latest_attempt, req).await
    } else if let Some(att) = latest_attempt {
        classify_without_request(ctx, call, &call_prefix, att).await
    } else {
        Ok(classify_unstarted_call(ctx, call))
    }
}

async fn classify_with_request(
    ctx: &RecoveryContext<'_>,
    call: &ToolCall,
    call_prefix: &str,
    latest_attempt: Option<&records::AttemptStart>,
    req: RequestBoundary<'_>,
) -> Result<CallRecovery> {
    let is_crashed_rerun =
        latest_attempt.is_some_and(|att| att.checkpoint.as_deref() == Some(req.checkpoint));

    if let Some(interrupted) =
        validate_checkpoint_exists(ctx.store, call_prefix, req.checkpoint).await?
    {
        return Ok(interrupted);
    }

    if is_crashed_rerun {
        if let Some(att) = latest_attempt {
            return classify_attempt_dispatches(ctx, call, &att.attempt_id, call_prefix).await;
        }
    }

    if req.chain_version == ctx.current_chain_version {
        Ok(CallRecovery::Resume)
    } else {
        let Some(store) = ctx.store else {
            return Ok(CallRecovery::Interrupted(
                "interrupted, outcome unknown: missing interaction checkpoint data: store absent"
                    .to_string(),
            ));
        };
        let call_id = call.id.as_deref().unwrap_or("");
        if store
            .call_has_dispatched(ctx.backend.session_id(), ctx.round, call_id)
            .await?
        {
            Ok(CallRecovery::Interrupted(
                "interrupted, outcome unknown: middleware changed while the call was waiting; downstream effects may already have happened".to_string(),
            ))
        } else {
            Ok(CallRecovery::Resume)
        }
    }
}

async fn classify_without_request(
    ctx: &RecoveryContext<'_>,
    call: &ToolCall,
    call_prefix: &str,
    att: &records::AttemptStart,
) -> Result<CallRecovery> {
    if let Some(ref cp_ref) = att.checkpoint {
        if let Some(interrupted) =
            validate_checkpoint_exists(ctx.store, call_prefix, cp_ref).await?
        {
            return Ok(interrupted);
        }
    }
    classify_attempt_dispatches(ctx, call, &att.attempt_id, call_prefix).await
}

async fn validate_checkpoint_exists(
    store: Option<&CheckpointStore>,
    call_prefix: &str,
    checkpoint_key: &str,
) -> Result<Option<CallRecovery>> {
    let Some(store) = store else {
        return Ok(Some(CallRecovery::Interrupted(
            "interrupted, outcome unknown: missing interaction checkpoint data: store absent"
                .to_string(),
        )));
    };
    match store
        .read_optional::<CallCheckpoint>(call_prefix, checkpoint_key)
        .await?
    {
        ReadCheckpoint::Found(_) => Ok(None),
        ReadCheckpoint::Missing(reason) => Ok(Some(CallRecovery::Interrupted(format!(
            "interrupted, outcome unknown: missing interaction checkpoint data: {reason}"
        )))),
    }
}

fn classify_unstarted_call(ctx: &RecoveryContext<'_>, call: &ToolCall) -> CallRecovery {
    if ctx.round_start_exists {
        return CallRecovery::Legacy;
    }
    let matches_middleware = ctx
        .chain
        .entries
        .iter()
        .any(|entry| entry.matcher.is_match(&call.name).unwrap_or(false));
    if matches_middleware {
        CallRecovery::Interrupted(
            "interrupted, outcome unknown: pre-interaction round with matching middleware"
                .to_string(),
        )
    } else {
        CallRecovery::Legacy
    }
}

async fn restore_completed(
    ctx: &RecoveryContext<'_>,
    call: &ToolCall,
    call_prefix: &str,
    public: &harnx_core::session::ToolOutput,
    private_snapshot: &str,
) -> Result<CallRecovery> {
    let Some(store) = ctx.store else {
        return Ok(CallRecovery::Interrupted(
            "interrupted, outcome unknown: missing interaction checkpoint data: store absent"
                .to_string(),
        ));
    };
    match store
        .read_optional::<ResultSnapshot>(call_prefix, private_snapshot)
        .await?
    {
        ReadCheckpoint::Missing(reason) => Ok(CallRecovery::Interrupted(format!(
            "interrupted, outcome unknown: missing interaction checkpoint data: {reason}"
        ))),
        ReadCheckpoint::Found(snapshot) => match snapshot.restore() {
            Ok(materialized) => {
                let result = materialized.result;
                if result.call == *call && public_output(&result) == *public {
                    Ok(CallRecovery::Completed(Box::new(result)))
                } else {
                    Ok(CallRecovery::Interrupted(
                        "interrupted, outcome unknown: completed tool snapshot identity mismatch"
                            .to_string(),
                    ))
                }
            }
            Err(err) => Ok(CallRecovery::Interrupted(format!(
                "interrupted, outcome unknown: snapshot restore failed: {err}"
            ))),
        },
    }
}

async fn classify_attempt_dispatches(
    ctx: &RecoveryContext<'_>,
    call: &ToolCall,
    attempt_id: &str,
    call_prefix: &str,
) -> Result<CallRecovery> {
    let Some(store) = ctx.store else {
        return Ok(CallRecovery::Resume);
    };

    let intent_pattern = format!("{call_prefix}attempts.{attempt_id}.dispatches.*.intent");
    let intent_keys: Vec<String> = store
        .keys(&intent_pattern)
        .await?
        .into_iter()
        .filter(|k| !k.contains(".chunks"))
        .collect();

    if intent_keys.is_empty() {
        return Ok(CallRecovery::Resume);
    }

    let mut downstream_completed = false;
    for intent_key in &intent_keys {
        let Some(physical_id) = extract_physical_id(intent_key) else {
            continue;
        };
        let completed_key =
            format!("{call_prefix}attempts.{attempt_id}.dispatches.{physical_id}.completed");
        if store.get(&completed_key).await?.is_some() {
            downstream_completed = true;
            break;
        }
        let call_id = call.id.as_deref().unwrap_or("");
        if check_journal_exact(ctx.backend, ctx.round, call_id, physical_id).await? {
            downstream_completed = true;
            break;
        }
    }

    if downstream_completed {
        Ok(CallRecovery::Interrupted(
            "interrupted, outcome unknown: the downstream tool completed but the middleware didn't finish"
                .to_string(),
        ))
    } else {
        Ok(CallRecovery::Interrupted(
            "interrupted, outcome unknown: the downstream outcome is unknown".to_string(),
        ))
    }
}

fn extract_physical_id(intent_key: &str) -> Option<&str> {
    let stripped = intent_key.strip_suffix(".intent")?;
    let prefix = ".dispatches.";
    let idx = stripped.rfind(prefix)?;
    Some(&stripped[idx + prefix.len()..])
}

async fn check_journal_exact(
    backend: &NatsSessionLogBackend,
    round: u64,
    tool_call_id: &str,
    physical_id: &str,
) -> Result<bool> {
    let js = backend.jetstream();
    let bucket_name = harnx_toolset_server::invocation_journal::BUCKET;
    match js.get_stream(format!("KV_{bucket_name}")).await {
        Ok(_) => {
            let kv = js.get_key_value(bucket_name).await?;
            let journal =
                harnx_toolset_server::invocation_journal::InvocationJournal::from_store(&js, kv);
            if let Some(record) = journal
                .find_exact(backend.session_id(), round, tool_call_id, physical_id)
                .await?
            {
                if record.reply.is_some() {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        Err(error)
            if matches!(
                error.kind(),
                async_nats::jetstream::context::GetStreamErrorKind::JetStream(ref error)
                    if error.kind() == async_nats::jetstream::ErrorCode::STREAM_NOT_FOUND
            ) =>
        {
            Ok(false)
        }
        Err(error) => Err(error).context("inspect invocation journal for recovery"),
    }
}
