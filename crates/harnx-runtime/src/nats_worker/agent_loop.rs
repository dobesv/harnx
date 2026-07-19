//! Agent loop entrypoint for NATS-backed sessions.

use super::backend::{FencedSessionLogSink, NatsSessionLogBackend};
use crate::agent_loop::OnToolRoundFn;
use crate::config::{GlobalConfig, Input};
use crate::nats_event_sink::NatsEventSink;
use crate::nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease};
use crate::nats_metrics;
use crate::nats_session_index::{put_record, SessionIndexRecord};
use crate::utils::AbortSignal;
use anyhow::{Context, Result};
use async_nats::jetstream;
use harnx_core::message::Message;
use harnx_core::session::SessionLogEntry;
use harnx_hooks::{AsyncHookManager, PersistentHookManager};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone)]
pub struct RunAgentLoopArgs<'a> {
    pub cluster_key: &'a str,
    pub session_id: &'a str,
    pub config: GlobalConfig,
    pub initial_input: Input,
    pub abort_signal: AbortSignal,
    pub call_fn: Option<crate::agent_loop::AgentCallFn>,
    pub lease: Option<Arc<NatsSessionLease>>,
    pub lease_config: NatsLeaseConfig,
    pub after_seq_observer: Option<Arc<AtomicU64>>,
    /// Optional observer of the JetStream seq of the header-insert migration
    /// EditEntries (S2), when the worker migrates a headerless remote session on
    /// this activation. The migration re-maps the leading-user block onto this
    /// seq, so the daemon must advance its activation high-water cursor to cover
    /// it — otherwise the end-of-turn drain re-folds the now-remapped (and
    /// already-answered) leading user messages and re-runs the turn (S3).
    pub header_insert_observer: Option<Arc<AtomicU64>>,
    /// Optional NATS KV store for the session index. When set, the worker
    /// upserts a `SessionIndexRecord` after the effective log has a `Header`
    /// on this activation, so remote sessions taking the existing-session path
    /// (headerless-migrated per S2, or normal resumes) are indexed and their
    /// `last_activity` is refreshed — not just brand-new empty-log sessions.
    pub session_index: Option<&'a async_nats::jetstream::kv::Store>,
    pub on_tool_round: Option<OnToolRoundFn>,
    pub working_dir: Option<std::path::PathBuf>,
}

impl<'a> RunAgentLoopArgs<'a> {
    pub fn with_lease(mut self, lease: Arc<NatsSessionLease>) -> Self {
        self.lease = Some(lease);
        self
    }

    pub fn with_after_seq_observer(mut self, observer: Arc<AtomicU64>) -> Self {
        self.after_seq_observer = Some(observer);
        self
    }

    pub fn with_header_insert_observer(mut self, observer: Arc<AtomicU64>) -> Self {
        self.header_insert_observer = Some(observer);
        self
    }
}

fn fold_user_messages(messages: &[Message]) -> String {
    messages
        .iter()
        .map(|message| message.content.to_text())
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[allow(dead_code)]
pub(crate) fn last_fed_user_log_seq(_input: &Input) -> Option<u64> {
    // DEPRECATED: This function cannot meaningfully derive a cursor from Input.
    // The cursor MUST be derived from the log seq of messages that went into
    // the turn input. Callers should use the `seed_cursor` returned by
    // `derive_turn_input` or pass messages directly.
    //
    // Kept for compatibility with resumable path which passes a synthesized
    // Input from resumable_ctx.last_user, but the cursor must come from
    // `resumable_ctx.last_user.log_seq` directly at the call site.
    #[allow(dead_code)]
    None
}

pub(crate) fn fold_new_user_messages_since(
    entries: &[(u64, harnx_core::session::SessionLogEntry)],
    cursor: Option<u64>,
) -> (Vec<Message>, Option<u64>) {
    // Apply mutations first: retracted/edited entries must be filtered.
    let effective_entries = match harnx_core::session_reconstruct::apply_log_mutations_nats(entries)
    {
        Ok(entries) => entries,
        Err(err) => {
            log::warn!("failed to apply NATS log mutations while folding user messages: {err}");
            return (Vec::new(), cursor);
        }
    };

    let mut messages = Vec::new();
    let mut latest_seq = cursor;
    for (seq, entry) in effective_entries {
        // Entry may be mutated; cursor semantics still skip seq <= cursor.
        if cursor.is_some_and(|seen| seq <= seen) {
            continue;
        }
        if let harnx_core::session::SessionLogEntry::Message {
            role,
            content,
            timestamp,
            ..
        } = entry
        {
            if role.is_user() {
                messages.push(
                    Message::new(role, content)
                        .with_log_seq(usize::try_from(seq).expect("JetStream seq fits usize"))
                        .with_log_timestamp(timestamp.unwrap_or_else(chrono::Utc::now)),
                );
                latest_seq = Some(seq);
            }
        }
    }
    (messages, latest_seq)
}

pub(crate) fn build_mid_turn_injection_callback(
    backend: NatsSessionLogBackend,
    cursor: Arc<AtomicU64>,
) -> OnToolRoundFn {
    Arc::new(move |merged_input, _results| {
        let backend = backend.clone();
        let cursor = Arc::clone(&cursor);
        Box::pin(async move {
            let tail = match backend.load_events_latest_async().await {
                Ok(entries) => entries,
                Err(err) => {
                    log::warn!("failed to reload session log for mid-turn injection: {err}");
                    return;
                }
            };
            let current = match cursor.load(std::sync::atomic::Ordering::SeqCst) {
                0 => None,
                seq => Some(seq),
            };
            let (messages, latest_seq) = fold_new_user_messages_since(&tail, current);
            if messages.is_empty() {
                return;
            }
            merged_input.set_injected_user_text(fold_user_messages(&messages));
            if let Some(seq) = latest_seq {
                cursor.store(seq, std::sync::atomic::Ordering::SeqCst);
            }
        })
    })
}

struct RepairOrphanToolCallsArgs<'a> {
    config: GlobalConfig,
    fence_token: Option<u64>,
    worker_id: Option<String>,
    session_id: &'a str,
    abort_signal: &'a AbortSignal,
}

/// Run the agent loop with a remote NATS session.
///
/// Connects to the given cluster, loads/replays the session from JetStream,
/// then runs `run_agent_loop` with persistence redirected to NATS.
///
/// For P1.3: single worker assumed sole owner (no HA/lease).
pub async fn run_agent_loop_with_nats(args: RunAgentLoopArgs<'_>) -> Result<()> {
    run_agent_loop_with_nats_inner(args).await
}

/// Like [`run_agent_loop_with_nats`], but fence-guarded by the holding lease
/// (P2.2). When `lease` is `Some`, every worker-originated append is gated on
/// `lease.is_held()` and stamped with the lease fence, and the session resume
/// is aborted if the persisted log tail already carries a fence GREATER than
/// the lease revision this worker holds (a newer worker has taken over).
pub async fn run_agent_loop_with_nats_inner(args: RunAgentLoopArgs<'_>) -> Result<()> {
    let RunAgentLoopArgs {
        cluster_key,
        session_id,
        config,
        initial_input,
        abort_signal,
        call_fn,
        lease,
        lease_config,
        after_seq_observer,
        header_insert_observer,
        session_index,
        on_tool_round,
        working_dir,
    } = args;
    // Get JetStream context from config (extract URL before await)
    // Connect through the config-driven helper so per-cluster auth/TLS
    // (token, require_tls, client cert, custom CA) is applied. Connecting with
    // a bare `async_nats::connect(url)` here would silently drop all auth/TLS
    // settings, breaking secure clusters. Snapshot the Config first so we don't
    // hold the lock across the await.
    let cfg_snapshot = config.read().clone();
    let jetstream_ctx = cfg_snapshot.nats_jetstream(cluster_key).await?;

    // Load or create session from NATS
    let mut backend = NatsSessionLogBackend::new(jetstream_ctx.clone(), session_id);
    if let Some(observer) = after_seq_observer {
        backend = backend.with_after_seq_observer(observer);
    }

    abort_resume_if_fenced(&backend, lease.as_deref())?;

    let session = load_or_repair_session(LoadOrRepairSessionParams {
        backend: &backend,
        config: &config,
        input: &initial_input,
        lease: lease.as_deref(),
        session_index,
        session_id,
        working_dir: working_dir.as_deref(),
        abort_signal: &abort_signal,
        header_insert_observer: header_insert_observer.as_ref(),
    })
    .await?;

    attach_session_to_config(&config, session, &backend, lease.as_ref());

    // Build AgentLoopContext
    let ctx = crate::agent_loop::AgentLoopContext {
        config: config.clone(),
        abort_signal: abort_signal.clone(),
        async_manager: Arc::new(tokio::sync::Mutex::new(AsyncHookManager::new())),
        persistent_manager: Arc::new(tokio::sync::Mutex::new(PersistentHookManager::new())),
        call_fn,
        on_tool_round,
        on_text_response: None,
        initial_with_embeddings: false,
        initial_resume_count: 0,
        max_resume: None,
        pending_async_context: None,
        working_dir,
        session_lock: None,
    };

    // Run unified agent loop
    // Persistence goes through shared Config.save_message entry construction; append_event routes sink
    run_agent_loop_segment(AgentLoopSegmentArgs {
        config,
        ctx,
        input: initial_input,
        abort_signal,
        jetstream_ctx,
        lease,
        lease_config,
        session_index: session_index.cloned(),
    })
    .await
}

struct AgentLoopSegmentArgs {
    config: GlobalConfig,
    ctx: crate::agent_loop::AgentLoopContext,
    input: Input,
    abort_signal: AbortSignal,
    jetstream_ctx: jetstream::Context,
    lease: Option<Arc<NatsSessionLease>>,
    lease_config: NatsLeaseConfig,
    session_index: Option<async_nats::jetstream::kv::Store>,
}

fn run_agent_loop_segment(
    args: AgentLoopSegmentArgs,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>> {
    Box::pin(async move {
        let AgentLoopSegmentArgs {
            config,
            ctx,
            input,
            abort_signal,
            jetstream_ctx,
            lease,
            lease_config,
            session_index,
        } = args;
        let loop_result = crate::agent_loop::run_agent_loop(&ctx, input).await?;
        match loop_result {
            crate::agent_loop::LoopResult::Completed => Ok(()),
            crate::agent_loop::LoopResult::HandoffRequested {
                agent,
                session_id,
                prompt,
            } => {
                let handoff_input = crate::config::input::from_str(&config, &prompt, None);
                let handoff_args = AgentLoopSegmentArgs {
                    config,
                    ctx,
                    input: handoff_input,
                    abort_signal,
                    jetstream_ctx,
                    lease,
                    lease_config,
                    session_index,
                };
                let (handoff_args, new_event_sink) =
                    prepare_nats_handoff(handoff_args, agent, session_id).await?;
                harnx_core::sink::with_agent_event_sink(new_event_sink, async move {
                    run_agent_loop_segment(handoff_args).await
                })
                .await
            }
        }
    })
}

async fn prepare_nats_handoff(
    mut args: AgentLoopSegmentArgs,
    agent: String,
    session_id: Option<String>,
) -> Result<(AgentLoopSegmentArgs, Arc<NatsEventSink>)> {
    let previous_lease = args
        .lease
        .as_ref()
        .context("NATS handoff requires active session lease")?;
    args.config
        .write()
        .exit_agent_with_lock(args.ctx.session_lock.as_ref())?;
    crate::config::Config::use_agent(
        &args.config,
        &agent,
        session_id.as_deref(),
        args.abort_signal.clone(),
    )
    .await?;
    let new_session_id = args
        .config
        .read()
        .session
        .as_ref()
        .and_then(|session| session.session_id.clone())
        .context("NATS handoff did not establish new session")?;
    let new_lease = NatsSessionLease::acquire(NatsLeaseAcquireParams {
        jetstream: args.jetstream_ctx.clone(),
        session_id: &new_session_id,
        worker_id: previous_lease.worker_id().to_string(),
        generation: previous_lease.generation(),
        config: args.lease_config.clone(),
        session_index: args.session_index.clone(),
    })
    .await?
    .with_context(|| {
        format!("Failed to acquire NATS lease for handed-off session '{new_session_id}'")
    })?;
    let new_lease = Arc::new(new_lease);
    let new_event_sink = Arc::new(
        NatsEventSink::new(
            args.jetstream_ctx.client().clone(),
            args.jetstream_ctx.clone(),
            new_session_id.clone(),
        )
        .await,
    );
    let new_after_seq_observer = new_event_sink.after_seq_handle();
    let mut new_backend = NatsSessionLogBackend::new(args.jetstream_ctx.clone(), &new_session_id);
    new_backend = new_backend.with_after_seq_observer(Arc::clone(&new_after_seq_observer));
    let new_session = args
        .config
        .read()
        .session
        .clone()
        .context("NATS handoff missing session after activation")?;
    attach_session_to_config(&args.config, new_session, &new_backend, Some(&new_lease));
    args.lease = Some(new_lease);
    Ok((args, new_event_sink))
}

/// Fence-on-resume fail-safe: if the persisted tail carries a worker fence
fn abort_resume_if_fenced(
    backend: &NatsSessionLogBackend,
    lease: Option<&NatsSessionLease>,
) -> Result<()> {
    let Some(lease) = lease else {
        return Ok(());
    };
    let entries: Vec<harnx_core::session::SessionLogEntry> = backend
        .load_events_blocking()?
        .into_iter()
        .map(|(_, e)| e)
        .collect();
    if let Some(max_fence) = harnx_core::session::max_worker_fence_token(&entries) {
        if max_fence > lease.fence_token() {
            anyhow::bail!(
                "aborting resume: log tail fence {max_fence} exceeds held lease revision {} (fenced by a newer worker)",
                lease.fence_token()
            );
        }
    }
    Ok(())
}

/// Parameters for [`load_or_repair_session`].
struct LoadOrRepairSessionParams<'a> {
    backend: &'a NatsSessionLogBackend,
    config: &'a GlobalConfig,
    input: &'a Input,
    lease: Option<&'a NatsSessionLease>,
    session_index: Option<&'a async_nats::jetstream::kv::Store>,
    session_id: &'a str,
    working_dir: Option<&'a std::path::Path>,
    abort_signal: &'a AbortSignal,
    /// When a header-insert migration is performed on this activation, its
    /// JetStream seq is published here so the daemon can advance its high-water
    /// cursor past the re-mapped leading-user block (S3).
    header_insert_observer: Option<&'a Arc<AtomicU64>>,
}

/// Load the session from the backend, creating a new one when the log is empty
/// or repairing orphan tool calls (with idempotency hints) when resuming an
/// existing session.
async fn load_or_repair_session(
    params: LoadOrRepairSessionParams<'_>,
) -> Result<harnx_core::session::Session> {
    let LoadOrRepairSessionParams {
        backend,
        config,
        input,
        lease,
        session_index,
        session_id,
        working_dir,
        abort_signal,
        header_insert_observer,
    } = params;
    let entries = backend.load_events_blocking()?;
    if entries.is_empty() {
        // New session: write header and load
        return write_header_and_load_session(
            backend,
            config,
            input,
            session_index,
            session_id,
            working_dir,
        )
        .await;
    }

    // Existing session: check effective session log for orphan tool calls and repair with hints
    let mut entries_vec = entries;
    let mut effective_entries =
        harnx_core::session_reconstruct::apply_log_mutations_nats(&entries_vec)?;
    maybe_insert_remote_header(MaybeInsertRemoteHeaderArgs {
        backend,
        config,
        input,
        session_id,
        working_dir,
        header_insert_observer,
        entries_vec: &mut entries_vec,
        effective_entries: &mut effective_entries,
    })
    .await?;
    refresh_session_index_on_activation(session_index, &effective_entries, session_id).await;
    let orphan_calls = find_orphan_tool_calls(&effective_entries);
    if !orphan_calls.is_empty() {
        nats_metrics::resume_detected();
        info!(
            "resume detected: session_id={} worker_id={} revision={} orphan_batches={}",
            session_id,
            lease.map(|l| l.worker_id()).unwrap_or("none"),
            lease.map(|l| l.fence_token()).unwrap_or(0),
            orphan_calls.len()
        );
        repair_orphan_tool_calls_with_hints(
            backend,
            &orphan_calls,
            RepairOrphanToolCallsArgs {
                config: config.clone(),
                fence_token: lease.map(|l| l.fence_token()),
                worker_id: lease.map(|l| l.worker_id().to_string()),
                session_id,
                abort_signal,
            },
        )
        .await?;
        // Repair appended ToolResults to the backend; reload so the
        // in-memory session reflects the repaired log rather than the
        // stale snapshot that still contains the orphan ToolCalls.
        entries_vec = backend.load_events_blocking()?;
    }
    crate::nats_session_log::load_session_from_entries(&entries_vec, session_id)
}

fn should_insert_remote_header(effective_entries: &[(u64, SessionLogEntry)]) -> bool {
    !effective_entries.iter().any(|(_, entry)| {
        matches!(
            entry,
            SessionLogEntry::Header { .. } | SessionLogEntry::Compress { .. }
        )
    })
}

fn build_remote_header_insert_replacements(
    raw_entries: &[(u64, SessionLogEntry)],
    effective_entries: &[(u64, SessionLogEntry)],
    config: &GlobalConfig,
    input: &Input,
    session_id: &str,
    working_dir: Option<&std::path::Path>,
) -> Result<Option<(usize, usize, Vec<String>)>> {
    if effective_entries.is_empty() || !should_insert_remote_header(effective_entries) {
        return Ok(None);
    }

    let mut first_user_seq = None;
    let mut last_user_seq = None;
    let mut replacements = Vec::new();
    replacements.push(serde_yaml::to_string(&build_remote_session_header(
        config,
        input,
        session_id,
        working_dir,
    )?)?);

    for (seq, entry) in raw_entries {
        match entry {
            SessionLogEntry::Message { role, .. } if role.is_user() => {
                let seq = usize::try_from(*seq)
                    .context("session log sequence does not fit into usize")?;
                if first_user_seq.is_none() {
                    first_user_seq = Some(seq);
                }
                last_user_seq = Some(seq);
                replacements.push(serde_yaml::to_string(entry)?);
            }
            _ if first_user_seq.is_some() => break,
            _ => return Ok(None),
        }
    }

    Ok(match (first_user_seq, last_user_seq) {
        (Some(from), Some(to)) => Some((from, to, replacements)),
        _ => None,
    })
}

fn remote_header_insert_message_id(session_id: &str, first_user_seq: usize) -> String {
    format!("{session_id}:header-insert:{first_user_seq}")
}

/// Attach the reconstructed session to the shared config with the NATS append
/// sink for the unified persistence path. With a lease, use the fence-guarded
/// sink so writes from a fenced-out worker are rejected.
fn attach_session_to_config(
    config: &GlobalConfig,
    mut session: harnx_core::session::Session,
    backend: &NatsSessionLogBackend,
    lease: Option<&Arc<NatsSessionLease>>,
) {
    let sink: Arc<dyn crate::config::session::SessionAppendSink> = match lease {
        Some(lease) => Arc::new(FencedSessionLogSink::new(
            backend.clone(),
            Arc::clone(lease),
        )),
        None => Arc::new(backend.clone()),
    };
    session.runtime = Some(Arc::new(sink));
    let mut cfg = config.write();
    cfg.session = Some(session);
}

/// Pending ToolCalls entry that lacks matching ToolResults.
#[allow(dead_code)]
struct PendingToolCalls {
    seq: u64,
    text: String,
    thought: Option<String>,
    calls: Vec<harnx_core::tool::ToolCall>,
    timestamp: Option<chrono::DateTime<chrono::Utc>>,
}

/// Decide whether an orphan tool call may be safely re-run during resume
/// repair. A tool is re-runnable when its declaration marks it idempotent or
/// read-only (MCP annotation hints). Unknown tools (absent from the map) are
/// treated as non-idempotent and must NOT be re-run.
pub(super) fn tool_can_rerun(
    decl_map: &std::collections::HashMap<String, harnx_core::tool::ToolDeclaration>,
    name: &str,
) -> bool {
    decl_map
        .get(name)
        .map(|d| d.idempotent_hint == Some(true) || d.read_only_hint == Some(true))
        .unwrap_or(false)
}

/// Find orphan tool calls in session log entries (trailing ToolCalls without matching ToolResults).
fn find_orphan_tool_calls(
    entries: &[(u64, harnx_core::session::SessionLogEntry)],
) -> Vec<PendingToolCalls> {
    use harnx_core::session::SessionLogEntry;

    // Find the last ToolCalls entry that lacks a matching ToolResults
    let mut orphans = Vec::new();
    let mut last_tool_calls: Option<PendingToolCalls> = None;

    for (seq, entry) in entries {
        match entry {
            SessionLogEntry::ToolCalls {
                text,
                thought,
                calls,
                timestamp,
                ..
            } => {
                last_tool_calls = Some(PendingToolCalls {
                    seq: *seq,
                    text: text.clone(),
                    thought: thought.clone(),
                    calls: calls.clone(),
                    timestamp: *timestamp,
                });
            }
            SessionLogEntry::ToolResults { .. } => {
                // ToolResults clears the pending ToolCalls
                last_tool_calls = None;
            }
            SessionLogEntry::Message { role, .. } if role.is_user() => {
                // A new user message ends the prior turn; any orphan ToolCalls above it is still an orphan
                if let Some(tc) = last_tool_calls.take() {
                    orphans.push(tc);
                }
            }
            _ => {}
        }
    }

    // If we end with a pending ToolCalls, that's the orphan
    if let Some(tc) = last_tool_calls {
        orphans.push(tc);
    }

    orphans
}

/// Repair orphan tool calls: for each orphan, re-run idempotent/readonly tools,
/// synthesize interrupt-error for non-idempotent ones.
/// All ToolResults are appended via the backend (fence-stamped when lease is held).
async fn repair_orphan_tool_calls_with_hints(
    backend: &NatsSessionLogBackend,
    orphan_calls: &[PendingToolCalls],
    args: RepairOrphanToolCallsArgs<'_>,
) -> Result<()> {
    use harnx_core::session::SessionLogEntry;

    let tool_repair = build_tool_repair_context(&args.config);
    let eval_ctx = build_orphan_tool_eval_context(&args.config, &tool_repair);

    for orphan in orphan_calls {
        let results = repair_single_orphan(orphan, &args, &tool_repair, &eval_ctx).await;
        let entry = apply_optional_fence_token(
            SessionLogEntry::ToolResults {
                results,
                timestamp: orphan.timestamp,
            },
            args.fence_token,
        );
        backend.append_event_blocking(&entry)?;
    }

    Ok(())
}

struct ToolRepairContext {
    decl_map: std::collections::HashMap<String, harnx_core::tool::ToolDeclaration>,
    agent_use_tools: Option<String>,
    current_agent_package: Option<String>,
    persistent_manager: Arc<tokio::sync::Mutex<PersistentHookManager>>,
}

fn build_tool_repair_context(config: &GlobalConfig) -> ToolRepairContext {
    let (decl_map, agent_use_tools, current_agent_package) = {
        let guard = config.read();
        let (tool_declarations, _) = guard.tool_declarations_for_use_tools(Some("*"), None);
        let decl_map = tool_declarations
            .into_iter()
            .map(|d| (d.name.clone(), d))
            .collect();
        let agent_use_tools = guard
            .agent
            .as_ref()
            .and_then(|a| a.use_tools().map(|v| v.join(",")));
        let current_agent_package = guard
            .agent
            .as_ref()
            .and_then(|a| harnx_core::package_namespace::pkg_from_qualified(a.name()))
            .map(str::to_string);
        (decl_map, agent_use_tools, current_agent_package)
    };

    ToolRepairContext {
        decl_map,
        agent_use_tools,
        current_agent_package,
        persistent_manager: Arc::new(tokio::sync::Mutex::new(PersistentHookManager::new())),
    }
}

fn build_orphan_tool_eval_context(
    config: &GlobalConfig,
    repair: &ToolRepairContext,
) -> crate::tool::ToolEvalContext {
    crate::tool::build_tool_eval_context(
        config,
        repair.agent_use_tools.as_deref(),
        repair.current_agent_package.clone(),
        &repair.persistent_manager,
        None,
    )
}

async fn repair_single_orphan(
    orphan: &PendingToolCalls,
    args: &RepairOrphanToolCallsArgs<'_>,
    repair: &ToolRepairContext,
    eval_ctx: &crate::tool::ToolEvalContext,
) -> Vec<harnx_core::session::ToolOutput> {
    let (mut results, rerun_calls) = partition_orphan_calls(orphan, args, repair);
    if !rerun_calls.is_empty() {
        let rerun_results =
            rerun_or_synthesize_tool_results(rerun_calls, eval_ctx, args.abort_signal).await;
        results.extend(rerun_results);
    }
    results
}

fn partition_orphan_calls(
    orphan: &PendingToolCalls,
    args: &RepairOrphanToolCallsArgs<'_>,
    repair: &ToolRepairContext,
) -> (
    Vec<harnx_core::session::ToolOutput>,
    Vec<harnx_core::tool::ToolCall>,
) {
    let mut results = Vec::new();
    let mut rerun_calls = Vec::new();

    for call in &orphan.calls {
        if tool_can_rerun(&repair.decl_map, &call.name) {
            log_resume_decision("rerun", args, call);
            rerun_calls.push(call.clone());
        } else {
            log_resume_decision("synthesize_interrupt_error", args, call);
            nats_metrics::interrupt_error_synthesized();
            results.push(interrupt_error_output(call));
        }
    }

    (results, rerun_calls)
}

async fn rerun_or_synthesize_tool_results(
    rerun_calls: Vec<harnx_core::tool::ToolCall>,
    eval_ctx: &crate::tool::ToolEvalContext,
    abort_signal: &AbortSignal,
) -> Vec<harnx_core::session::ToolOutput> {
    match crate::tool::eval_tool_calls(eval_ctx, rerun_calls.clone(), abort_signal).await {
        Ok(tool_results) => tool_results
            .into_iter()
            .map(|result| harnx_core::session::ToolOutput {
                id: result.call.id.clone(),
                name: result.call.name.clone(),
                output: result.output,
                markdown: result.markdown,
                content: result.content,
                switch_agent: result.switch_agent,
            })
            .collect(),
        Err(err) => rerun_calls
            .into_iter()
            .map(|call| rerun_failure_output(&call, &err))
            .collect(),
    }
}

fn apply_optional_fence_token(
    mut entry: harnx_core::session::SessionLogEntry,
    fence_token: Option<u64>,
) -> harnx_core::session::SessionLogEntry {
    if let Some(fence_token) = fence_token {
        entry.set_fence_token(fence_token);
    }
    entry
}

/// Log a per-call resume decision (`rerun` or `synthesize_interrupt_error`).
fn log_resume_decision(
    decision: &str,
    args: &RepairOrphanToolCallsArgs<'_>,
    call: &harnx_core::tool::ToolCall,
) {
    info!(
        "resume decision {decision}: session_id={} worker_id={} revision={} tool_name={} call_id={}",
        args.session_id,
        args.worker_id.as_deref().unwrap_or("none"),
        args.fence_token.unwrap_or(0),
        call.name,
        call.id.as_deref().unwrap_or("none")
    );
}

fn interrupt_error_output(call: &harnx_core::tool::ToolCall) -> harnx_core::session::ToolOutput {
    harnx_core::session::ToolOutput {
        id: call.id.clone(),
        name: call.name.clone(),
        output: serde_json::json!({
            "error": "tool response lost (session was interrupted before results were persisted)"
        }),
        markdown: None,
        content: Vec::new(),
        switch_agent: None,
    }
}

fn rerun_failure_output(
    call: &harnx_core::tool::ToolCall,
    err: &anyhow::Error,
) -> harnx_core::session::ToolOutput {
    harnx_core::session::ToolOutput {
        id: call.id.clone(),
        name: call.name.clone(),
        output: serde_json::json!({
            "error": format!("tool re-run failed: {err:#}")
        }),
        markdown: None,
        content: Vec::new(),
        switch_agent: None,
    }
}

fn build_remote_session_header(
    config: &GlobalConfig,
    input: &Input,
    session_id: &str,
    working_dir: Option<&std::path::Path>,
) -> Result<harnx_core::session::SessionLogEntry> {
    let mut header_session = crate::config::session::new(&config.read(), session_id, working_dir)?;
    header_session.set_agent(&input.agent)?;
    Ok(header_session.build_header_entry())
}

fn build_session_index_record_from_header(
    header: &harnx_core::session::SessionLogEntry,
    title: Option<String>,
) -> Result<SessionIndexRecord> {
    let harnx_core::session::SessionLogEntry::Header {
        session_id,
        agent_name,
        working_dir,
        git_branch,
        git_remote,
        ..
    } = header
    else {
        anyhow::bail!("remote session index requires header entry")
    };

    Ok(SessionIndexRecord {
        session_id: session_id
            .clone()
            .context("remote session header missing session_id")?,
        agent_name: agent_name
            .clone()
            .context("remote session header missing agent_name")?,
        working_dir: working_dir.clone(),
        git_branch: git_branch.clone(),
        git_remote: git_remote.clone(),
        title,
        last_activity: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock before unix epoch")?
            .as_secs(),
    })
}

/// Scan effective log entries for the most recent `Title` event so the session
/// index reflects the latest generated/manual title on activation upsert.
fn latest_title_from_entries(
    entries: &[(u64, harnx_core::session::SessionLogEntry)],
) -> Option<String> {
    entries.iter().rev().find_map(|(_, entry)| match entry {
        harnx_core::session::SessionLogEntry::Title { title, .. } => Some(title.clone()),
        _ => None,
    })
}

async fn upsert_session_index_record(
    store: &async_nats::jetstream::kv::Store,
    header: &harnx_core::session::SessionLogEntry,
    title: Option<String>,
) -> Result<u64> {
    let record = build_session_index_record_from_header(header, title)?;
    put_record(store, &record)
        .await
        .with_context(|| format!("put session index record for {}", record.session_id))
}

/// Keep the session index current on every activation that takes the
/// existing-session path. Brand-new (empty-log) sessions register their index in
/// `write_header_and_load_session`; but headerless sessions migrated here (S2)
/// and normal resumes take THIS path, so we upsert from the effective `Header`
/// (carrying the latest `Title`) — otherwise the session never appears in
/// `list_remote_sessions_with_meta` (breaks resume/picker) and its
/// `last_activity` is never refreshed. Best-effort: warn on failure, never fail
/// activation. Idempotent (`put_record` upserts).
async fn refresh_session_index_on_activation(
    session_index: Option<&async_nats::jetstream::kv::Store>,
    effective_entries: &[(u64, SessionLogEntry)],
    session_id: &str,
) {
    let Some(store) = session_index else {
        return;
    };
    let Some((_, header)) = effective_entries
        .iter()
        .find(|(_, entry)| matches!(entry, SessionLogEntry::Header { .. }))
    else {
        return;
    };
    let title = latest_title_from_entries(effective_entries);
    if let Err(err) = upsert_session_index_record(store, header, title).await {
        log::warn!(
            "failed to upsert remote session index during activation: \
             session_id={session_id} err={err:#}"
        );
    }
}

struct MaybeInsertRemoteHeaderArgs<'a> {
    backend: &'a NatsSessionLogBackend,
    config: &'a GlobalConfig,
    input: &'a Input,
    session_id: &'a str,
    working_dir: Option<&'a std::path::Path>,
    header_insert_observer: Option<&'a Arc<AtomicU64>>,
    entries_vec: &'a mut Vec<(u64, SessionLogEntry)>,
    effective_entries: &'a mut Vec<(u64, SessionLogEntry)>,
}

/// Headerless sessions migrated in (S2) need a `Header` synthesized from the
/// leading user block. When required, this appends an `EditEntries` migration,
/// publishes the resulting seq to the daemon's high-water observer, and reloads
/// `entries_vec` / `effective_entries` so the caller sees the repaired log.
/// No-op when the effective log already has a header.
async fn maybe_insert_remote_header(args: MaybeInsertRemoteHeaderArgs<'_>) -> Result<()> {
    let MaybeInsertRemoteHeaderArgs {
        backend,
        config,
        input,
        session_id,
        working_dir,
        header_insert_observer,
        entries_vec,
        effective_entries,
    } = args;

    if !should_insert_remote_header(effective_entries) {
        return Ok(());
    }
    let Some((first_user_seq, last_user_seq, replacements)) =
        build_remote_header_insert_replacements(
            entries_vec,
            effective_entries,
            config,
            input,
            session_id,
            working_dir,
        )?
    else {
        return Ok(());
    };

    let edit_entry = SessionLogEntry::EditEntries {
        from: first_user_seq,
        to: last_user_seq,
        replacements,
    };
    let message_id = remote_header_insert_message_id(session_id, first_user_seq);
    let insert_seq =
        crate::nats_session_log::NatsSessionLog::new(backend.jetstream(), session_id.to_string())
            .append_event_with_message_id_async(&edit_entry, message_id)
            .await?;
    debug!("inserted header via EditEntries js{insert_seq}");
    // Publish the migration seq so the daemon advances its activation high-water
    // cursor past the re-mapped leading-user block. The migration re-maps those
    // users onto `insert_seq`; the turn that runs this activation answers them,
    // so without this the drain would re-fold them (seq > pre-migration cursor)
    // and re-run the turn (S3).
    if let Some(observer) = header_insert_observer {
        observer.fetch_max(insert_seq, std::sync::atomic::Ordering::Relaxed);
    }
    *entries_vec = backend.load_events_blocking()?;
    *effective_entries = harnx_core::session_reconstruct::apply_log_mutations_nats(entries_vec)?;
    Ok(())
}

/// Write a new session header and load the session.
pub(crate) async fn write_header_and_load_session(
    backend: &NatsSessionLogBackend,
    config: &GlobalConfig,
    input: &Input,
    session_index: Option<&async_nats::jetstream::kv::Store>,
    session_id: &str,
    working_dir: Option<&std::path::Path>,
) -> Result<harnx_core::session::Session> {
    let header = build_remote_session_header(config, input, session_id, working_dir)?;
    backend.append_event_blocking(&header)?;
    if let Some(store) = session_index {
        if let Err(err) = upsert_session_index_record(store, &header, None).await {
            log::warn!(
                "failed to upsert remote session index after header write: session_id={} err={err:#}",
                session_id
            );
        }
    }
    let entries = backend.load_events_blocking()?;
    crate::nats_session_log::load_session_from_entries(&entries, session_id)
}

#[cfg(test)]
mod tests {
    use super::fold_new_user_messages_since;
    use chrono::{TimeZone, Utc};
    use harnx_core::message::{MessageContent, MessageRole};
    use harnx_core::session::SessionLogEntry;

    fn user_entry(id: &str, text: &str, timestamp: chrono::DateTime<Utc>) -> SessionLogEntry {
        SessionLogEntry::Message {
            id: Some(id.to_string()),
            role: MessageRole::User,
            content: MessageContent::Text(text.to_string()),
            timestamp: Some(timestamp),
            fence_token: None,
        }
    }

    #[test]
    fn fold_new_user_messages_since_excludes_retracted_messages() {
        let entries = vec![
            (
                1,
                user_entry(
                    "msg-1",
                    "retracted message",
                    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 1).unwrap(),
                ),
            ),
            (
                2,
                SessionLogEntry::EditEntries {
                    from: 1,
                    to: 1,
                    replacements: vec![],
                },
            ),
            (
                3,
                user_entry(
                    "msg-3",
                    "valid message",
                    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 3).unwrap(),
                ),
            ),
        ];

        let (messages, latest_seq) = fold_new_user_messages_since(&entries, None);

        assert_eq!(messages.len(), 1, "retracted message must be excluded");
        assert_eq!(messages[0].content.to_text(), "valid message");
        assert_eq!(messages[0].log_seq, Some(3));
        assert_eq!(
            messages[0].log_timestamp,
            Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 3).unwrap())
        );
        assert_eq!(latest_seq, Some(3));
    }

    #[test]
    fn fold_new_user_messages_since_skips_non_user_entries_but_tracks_latest_user_seq() {
        let entries = vec![
            (
                1,
                user_entry(
                    "msg-1",
                    "first valid",
                    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 1).unwrap(),
                ),
            ),
            (
                2,
                SessionLogEntry::Message {
                    id: Some("assistant-2".to_string()),
                    role: MessageRole::Assistant,
                    content: MessageContent::Text("assistant reply".to_string()),
                    timestamp: Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 2).unwrap()),
                    fence_token: None,
                },
            ),
            (
                3,
                user_entry(
                    "msg-3",
                    "second valid",
                    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 3).unwrap(),
                ),
            ),
        ];

        let (messages, latest_seq) = fold_new_user_messages_since(&entries, None);

        let folded: Vec<_> = messages
            .iter()
            .map(|message| {
                (
                    message.content.to_text(),
                    message.log_seq,
                    message.log_timestamp,
                )
            })
            .collect();
        assert_eq!(
            folded,
            vec![
                (
                    "first valid".to_string(),
                    Some(1),
                    Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 1).unwrap())
                ),
                (
                    "second valid".to_string(),
                    Some(3),
                    Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 3).unwrap())
                ),
            ]
        );
        assert_eq!(
            latest_seq,
            Some(3),
            "latest_seq must be max consumed user-message seq"
        );
    }

    #[test]
    fn fold_new_user_messages_since_cursor_semantics_with_retracts() {
        let entries = vec![
            (
                1,
                user_entry(
                    "msg-1",
                    "retracted",
                    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 1).unwrap(),
                ),
            ),
            (
                2,
                SessionLogEntry::EditEntries {
                    from: 1,
                    to: 1,
                    replacements: vec![],
                },
            ),
            (
                3,
                user_entry(
                    "msg-3",
                    "first valid",
                    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 3).unwrap(),
                ),
            ),
            (
                4,
                user_entry(
                    "msg-4",
                    "second valid",
                    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 4).unwrap(),
                ),
            ),
        ];

        let (messages, latest_seq) = fold_new_user_messages_since(&entries, Some(3));

        assert_eq!(
            messages.len(),
            1,
            "entries with seq <= cursor must be skipped after mutations"
        );
        assert_eq!(messages[0].content.to_text(), "second valid");
        assert_eq!(
            messages[0].log_seq,
            Some(4),
            "returned message must preserve original seq for stamping"
        );
        assert_eq!(
            messages[0].log_timestamp,
            Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 4).unwrap())
        );
        assert_eq!(
            latest_seq,
            Some(4),
            "latest_seq must track max consumed user-message seq"
        );
    }
}
