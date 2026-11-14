//! Live progress for dispatched prompts.
//!
//! A synchronous `session_prompt` reports child progress from the in-process
//! turn. A dispatched prompt runs on another worker, so the parent instead
//! spawns a watcher here on successful dispatch (and again from an
//! authorized `session_wait`, deduplicated per prompt). The watcher:
//!
//! 1. binds the prompt's durable admission BEFORE announcing anything — an
//!    unknown prompt must not appear as `Running`, and a prompt that already
//!    settled gets a single terminal snapshot instead of a `Running` row that
//!    immediately flips;
//! 2. starts the shared `SubagentProgressReporter`, which announces `Running`
//!    immediately, keeps the heartbeat/title cadence, publishes to the
//!    parent's advisory stream, and shares every snapshot with this module;
//! 3. subscribes to the child's advisory fan-out and feeds every child
//!    `AgentEvent` into the reporter sink, reusing the sync path's
//!    usage/tool-count aggregation;
//! 4. polls the child's durable log through an incremental scan (the cursor
//!    survives each tick, so a large child log is read once) and finishes the
//!    reporter only from the prompt's durable terminal evidence, replacing the
//!    usage estimate with the `TurnEnd` total.
//!
//! A dispatch receipt and a soft-wait expiry are tool results, not child
//! progress: the watcher keeps running past both, so terminal rows only ever
//! come from real settlement.
//!
//! Lifetime: a watcher ends when its prompt durably settles, when the prompt
//! binding is already gone at bind time (an unknown prompt announces nothing;
//! the binding is not re-checked later), or when the scan can no longer prove
//! the prompt's history (permanent evidence gap); it never outlives the
//! process that spawned it. That is the one known limitation: a parent
//! process that restarts while its dispatched children still run loses their
//! live rows — a later `session_wait` re-creates the watcher for prompts it
//! observes, and the settled wait result still carries the terminal summary
//! for replay. Watchers of prompts nothing ever waits on end with their
//! process; they are not persisted or resumed. Advisories are lossy by
//! contract, including the terminal publish: the one retry covers a reporter
//! that stopped before flushing, and a loss inside that window is accepted —
//! the durable summary the next wait returns is authoritative.

use super::observation::{Stop, WatchBindError, WatchScan, WatchState, WatchTarget};
use super::SessionPromptStatus;
use crate::nats_event_sink::{events_subject, AdvisoryEnvelope, NatsEventSink};
use crate::nats_worker::subagent_progress::{
    ReportedInvocation, SharedSnapshots, SubagentProgressReporter,
};
use futures_util::StreamExt;
use harnx_core::api_types::CompletionTokenUsage;
use harnx_core::event::{
    AgentEvent, AgentSource, SubAgentProgress, SubAgentProgressStatus, TurnEvent,
};
use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use super::super::SubagentToolset;

/// The dispatched prompt a watcher follows, in the terms the caller already
/// proved at dispatch or observation.
pub(super) struct WatchedPrompt {
    /// Parent storage key: the advisory subject progress publishes to, the
    /// same subject the synchronous reporter uses.
    pub(super) parent: String,
    /// Child agent name for the progress source.
    pub(super) agent: String,
    /// Child local session id: the progress `session_id`, a presentation
    /// name only. Subjects use the child's storage key.
    pub(super) session_id: String,
    /// The dispatched prompt's durable identity. Doubles as the progress
    /// invocation id so a dispatch and the later waits that observe it agree
    /// on one row per child invocation.
    pub(super) prompt_id: String,
    /// Parent transcript id of the calling tool (`session_send` and
    /// `session_wait` both receive it as a hidden transport argument), when
    /// the transport supplied one. Labels the child's row on the parent call.
    pub(super) tool_call_id: Option<String>,
}

/// Child identity for building a progress snapshot: presentation names.
/// Subjects and durable keys use the storage key; this is what rows and
/// results display.
pub(super) struct ChildIdentity {
    pub(super) agent: String,
    pub(super) session_id: String,
    pub(super) prompt_id: String,
}

/// Watchers this process is running, so a `session_wait` on a prompt
/// dispatched in the same process does not double-report it, and the settled
/// observation can read the live view. Keyed by parent scope, child storage
/// and prompt id; the value is the watcher's shared snapshot cell, whose
/// `None`-to-`Some` transition proves the reporter started.
fn active_watchers() -> &'static Mutex<HashMap<String, SharedSnapshots>> {
    static WATCHERS: OnceLock<Mutex<HashMap<String, SharedSnapshots>>> = OnceLock::new();
    WATCHERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn watcher_key(parent: &str, storage: &str, prompt_id: &str) -> String {
    format!("{parent}\u{1f}{storage}\u{1f}{prompt_id}")
}

/// Terminal snapshots of watchers that already ended, bounded so the process
/// cannot grow without limit. When a watcher finishes, its registry entry
/// goes away; without this cache a later same-process `session_wait` would
/// start a fresh empty cell and zero out the measured view (title, tool
/// count, elapsed, usage estimate).
const FINISHED_SNAPSHOT_LIMIT: usize = 128;

struct FinishedSnapshots {
    map: HashMap<String, SubAgentProgress>,
    order: VecDeque<String>,
}

impl FinishedSnapshots {
    fn retain(&mut self, key: String, snapshot: SubAgentProgress) {
        if !self.map.contains_key(&key) {
            self.order.push_back(key.clone());
            if self.order.len() > FINISHED_SNAPSHOT_LIMIT {
                let evicted = self.order.pop_front().expect("order tracks map insertion");
                self.map.remove(&evicted);
            }
        }
        self.map.insert(key, snapshot);
    }

    /// The retained terminal snapshot of a prompt this process already
    /// settled, if one survived the bounded eviction.
    fn finished(&self, key: &str) -> Option<SubAgentProgress> {
        self.map.get(key).cloned()
    }

    /// An empty cache, isolated from the process-global one: tests exercise
    /// the retention rules here without racing other tests' evictions.
    #[cfg(test)]
    fn empty() -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }
}

fn finished_snapshots() -> &'static Mutex<FinishedSnapshots> {
    static FINISHED: OnceLock<Mutex<FinishedSnapshots>> = OnceLock::new();
    FINISHED.get_or_init(|| {
        Mutex::new(FinishedSnapshots {
            map: HashMap::new(),
            order: VecDeque::new(),
        })
    })
}

/// Retain a watcher's terminal snapshot after its registry entry is gone,
/// keyed by `watcher_key` (parent scope, child storage, prompt) — the same
/// identity the active registry uses — so the measured view survives into
/// later same-process waits by the SAME parent scope. The parent component
/// keeps unrelated scopes (and test fixtures, which share child storage
/// keys and prompt ids) from prefilling each other's cells with a terminal
/// snapshot of a different run; a scope that never ran a watcher here falls
/// back to the sparse durable summary, never a wrong measured view.
fn retain_finished(parent: &str, storage: &str, prompt_id: &str, snapshot: SubAgentProgress) {
    retain_finished_in(
        &mut finished_snapshots()
            .lock()
            .expect("finished watcher snapshots"),
        watcher_key(parent, storage, prompt_id),
        snapshot,
    );
}

/// The retention rule, independent of which cache it runs against: only
/// terminal snapshots are kept — a `Running` view would be prefilled into a
/// later wait's cell and resurrect a row the watcher already abandoned (e.g.
/// after the reporter's actor died before its terminal publish, the share
/// still holds the last running view).
fn retain_finished_in(cache: &mut FinishedSnapshots, key: String, snapshot: SubAgentProgress) {
    if snapshot.status == SubAgentProgressStatus::Running {
        log::debug!("not retaining a Running snapshot under '{key}'");
        return;
    }
    cache.retain(key, snapshot);
}

fn finished_snapshot(parent: &str, storage: &str, prompt_id: &str) -> Option<SubAgentProgress> {
    finished_snapshots()
        .lock()
        .expect("finished watcher snapshots")
        .finished(&watcher_key(parent, storage, prompt_id))
}

/// Removes a watcher's registry entry when its task ends, however it ends —
/// normal return, panic, or runtime shutdown — so a later wait on the same
/// prompt starts a fresh watcher instead of shadowing a dead entry. Removal
/// is keyed to this watcher's shared cell: if a replacement was already
/// registered under the same key, it stays.
struct RegistryGuard {
    key: String,
    shared: std::sync::Weak<Mutex<Option<SubAgentProgress>>>,
}

impl Drop for RegistryGuard {
    fn drop(&mut self) {
        let shared = match self.shared.upgrade() {
            Some(shared) => shared,
            None => return,
        };
        let mut registry = active_watchers().lock().expect("watcher registry");
        if registry
            .get(&self.key)
            .is_some_and(|registered| std::sync::Arc::ptr_eq(registered, &shared))
        {
            registry.remove(&self.key);
        }
    }
}

/// Start (or join) this process's watcher for a dispatched prompt. The
/// returned shared cell carries the watcher's latest published snapshot; a
/// `session_wait` whose process never dispatched the prompt gets a fresh
/// watcher, prefilled with the retained terminal snapshot when an earlier
/// watcher of the same parent scope and prompt already finished here.
pub(super) fn ensure_watcher(toolset: &SubagentToolset, watched: WatchedPrompt) -> SharedSnapshots {
    let storage =
        harnx_core::session_identity::session_key(Some(&watched.agent), &watched.session_id);
    let key = watcher_key(&watched.parent, &storage, &watched.prompt_id);
    let mut registry = active_watchers().lock().expect("watcher registry");
    if let Some(shared) = registry.get(&key) {
        return std::sync::Arc::clone(shared);
    }
    // An earlier watcher of this prompt already finished in this process:
    // its retained terminal snapshot is the live view, so the settled
    // summary of this wait keeps the measured fields instead of zeroes.
    let retained = finished_snapshot(&watched.parent, &storage, &watched.prompt_id);
    let shared: SharedSnapshots = std::sync::Arc::new(Mutex::new(retained));
    registry.insert(key.clone(), std::sync::Arc::clone(&shared));
    drop(registry);
    let config = WatcherConfig {
        client: toolset.client.clone(),
        jetstream: toolset.jetstream.clone(),
        session_metadata: toolset.session_metadata.clone(),
        heartbeat: toolset.progress_heartbeat,
        poll: toolset.progress_poll,
        pass_limit: toolset
            .watcher_pass_limit
            .unwrap_or(super::observation::IO_LIMIT),
        storage,
        watched,
    };
    let guarded = std::sync::Arc::downgrade(&shared);
    let task_shared = std::sync::Arc::clone(&shared);
    tokio::spawn(async move {
        let _guard = RegistryGuard {
            key,
            shared: guarded,
        };
        run_watcher(config, task_shared).await;
    });
    shared
}

/// Test seam: whether a watcher is currently registered for this prompt. The
/// watcher task's `RegistryGuard` removes the entry when it ends, so tests can
/// assert a watcher stopped instead of leaking.
#[cfg(test)]
pub(super) fn watcher_registered(
    parent: &str,
    agent: &str,
    session_id: &str,
    prompt_id: &str,
) -> bool {
    let storage = harnx_core::session_identity::session_key(Some(agent), session_id);
    let key = watcher_key(parent, &storage, prompt_id);
    active_watchers()
        .lock()
        .expect("watcher registry")
        .contains_key(&key)
}

struct WatcherConfig {
    client: async_nats::Client,
    jetstream: async_nats::jetstream::Context,
    session_metadata: crate::nats_session_metadata::SessionMetadataStore,
    heartbeat: Duration,
    poll: Duration,
    /// Per-pass I/O bound of the durable scan; `IO_LIMIT` unless a test pinned
    /// a smaller one.
    pass_limit: Duration,
    storage: String,
    watched: WatchedPrompt,
}

/// How many poll intervals the initial binding may keep failing on transient
/// broker errors before the watcher gives up and frees its registry key.
const BIND_ATTEMPTS: usize = 3;

async fn run_watcher(config: WatcherConfig, shared: SharedSnapshots) {
    // Out-of-band publisher: the parent's own writer keeps appending durable
    // entries while this watcher publishes, so the sink must re-stamp
    // `after_seq` per publish or caught-up clients fence every advisory.
    let parent_sink = NatsEventSink::with_live_seq_tracking(
        config.client.clone(),
        config.jetstream.clone(),
        config.watched.parent.clone(),
    )
    .await;
    let Some(scan) = bind_and_first_refresh(&config, &parent_sink, &shared).await else {
        return;
    };
    // `ensure_watcher` prefilled `shared` with the retained terminal
    // snapshot of a prompt this process already settled; a scan that has
    // not confirmed it YET does not make that state stale. Announcing
    // Running here would overwrite the measured terminal view with zeroes
    // and resurrect a finished row until a later pass confirms (R2-N6), so
    // wait for the durable confirmation instead of starting a reporter.
    if retained_terminal(&shared) {
        watch_for_reconfirmation(config, scan, parent_sink, shared).await;
        return;
    }
    let reporter = announce_watcher(&config, &parent_sink, &shared).await;
    let subscriber = subscribe_child_events(&config).await;
    watch_until_settled(Watching {
        config,
        scan,
        reporter,
        subscriber,
        parent_sink,
        shared,
        refresh_failures: 0,
    })
    .await;
}

/// Bind the durable scan and take its first refresh, resolving every outcome
/// that ends the watcher before any announcement. `None` means the watcher
/// is done (binding gone, bind failures, an already-settled prompt
/// republished in one shot, or a bind-time evidence gap); `Some(scan)` means
/// the prompt is still provable and the caller decides how to announce.
async fn bind_and_first_refresh(
    config: &WatcherConfig,
    parent_sink: &NatsEventSink,
    shared: &SharedSnapshots,
) -> Option<WatchScan> {
    // `Ok(None)` means the prompt binding is gone: nothing left to follow,
    // and nothing was announced yet.
    let mut scan = bind_scan(config).await.ok().flatten()?;
    // A prompt that already settled before this watcher bound it (a
    // `session_wait` on a completed child) gets its terminal row in one shot,
    // without announcing `Running` first and without a heartbeat actor whose
    // row would immediately flip terminal. A bind-time evidence gap announces
    // nothing either: the row could never finish.
    match scan.refresh().await {
        Ok(terminal @ WatchState::Terminal { .. }) => {
            publish_settled_progress(&config.watched, parent_sink, terminal, shared).await;
            return None;
        }
        Ok(WatchState::EvidenceError(evidence)) => {
            log::warn!(
                "stop sub-agent progress watcher: prompt '{}' of '{}' \
                 has a permanent evidence gap ({evidence}) before announcing",
                config.watched.prompt_id,
                config.watched.session_id
            );
            return None;
        }
        // Blocked on an approval: the poll loop retries the scan.
        Ok(WatchState::Pending) => {}
        // A transient read failure: same retry, and the poll loop keeps its
        // cursor.
        Err(stop) => log::debug!(
            "initial durable refresh for dispatched prompt '{}' of '{}' failed ({stop:?}); continuing with polling",
            config.watched.prompt_id,
            config.watched.session_id
        ),
    }
    Some(scan)
}

/// The wait path's prefilled view of a prompt this process already settled:
/// a terminal snapshot retained by an earlier watcher of it. Such a share
/// suppresses the Running announcement a fresh watcher would otherwise make.
fn retained_terminal(shared: &SharedSnapshots) -> bool {
    shared
        .lock()
        .expect("watcher share")
        .as_ref()
        .is_some_and(|snapshot| snapshot.status != SubAgentProgressStatus::Running)
}

/// Re-confirm a prompt this process already settled (its retained terminal
/// snapshot is the live view in `shared`): poll the durable log until it
/// republishes the terminal snapshot — one row refresh for the parent's live
/// view — or a permanent evidence gap ends the task with the retained
/// snapshot still in place. No reporter starts here: the row is terminal, so
/// no Running announcement may precede the confirmation.
async fn watch_for_reconfirmation(
    config: WatcherConfig,
    mut scan: WatchScan,
    parent_sink: NatsEventSink,
    shared: SharedSnapshots,
) {
    let mut poll = tokio::time::interval(config.poll);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut refresh_failures = 0u32;
    loop {
        poll.tick().await;
        #[cfg(test)]
        count_reconfirmation_pass();
        match scan.refresh().await {
            Ok(terminal @ WatchState::Terminal { .. }) => {
                publish_settled_progress(&config.watched, &parent_sink, terminal, &shared).await;
                return;
            }
            Ok(WatchState::EvidenceError(evidence)) => {
                log::warn!(
                    "stop sub-agent progress watcher: prompt '{}' of '{}' \
                     has a permanent evidence gap ({evidence}) after an earlier settlement",
                    config.watched.prompt_id,
                    config.watched.session_id
                );
                return;
            }
            // The durable log is the truth but one bounded pass may lag
            // behind it; the cursor survives, so the next tick resumes.
            Ok(WatchState::Pending) => {}
            // Transient read failure: warn once, then debug, so a long
            // outage is visible without spamming — same cadence as the
            // live watcher's `note_refresh_failure`.
            Err(stop @ (Stop::Expired | Stop::Unavailable)) => {
                refresh_failures += 1;
                if refresh_failures == 1 {
                    log::warn!(
                        "durable refresh for settled prompt '{}' of '{}' failed ({stop:?}); \
                         retrying on the next poll",
                        config.watched.prompt_id,
                        config.watched.session_id
                    );
                } else {
                    log::debug!(
                        "durable refresh for settled prompt '{}' of '{}' still failing \
                         ({stop:?}, attempt {})",
                        config.watched.prompt_id,
                        config.watched.session_id,
                        refresh_failures
                    );
                }
            }
            // Unreachable today: the per-pass budget has a fresh,
            // never-cancelled token and no caller stops, so refresh cannot
            // yield these. If that ever changes, stop and keep the retained
            // snapshot instead of spinning.
            Err(stop) => {
                log::warn!(
                    "stop durable refresh for settled prompt '{}' of '{}': {stop:?}",
                    config.watched.prompt_id,
                    config.watched.session_id
                );
                return;
            }
        }
    }
}

/// Test observation (R6-B1): passes the reconfirmation loop has taken, so
/// a test can prove the retained-terminal branch entered it. Counted per
/// process; `nextest` isolates each test in its own process, so no reset.
#[cfg(test)]
pub(in crate::nats_worker::subagent_toolset) fn reconfirmation_passes() -> usize {
    reconfirmation_pass_counter().load(std::sync::atomic::Ordering::SeqCst)
}

#[cfg(test)]
fn count_reconfirmation_pass() {
    reconfirmation_pass_counter().fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(test)]
fn reconfirmation_pass_counter() -> &'static std::sync::atomic::AtomicUsize {
    static PASSES: std::sync::OnceLock<std::sync::atomic::AtomicUsize> = std::sync::OnceLock::new();
    PASSES.get_or_init(|| std::sync::atomic::AtomicUsize::new(0))
}

/// Announces the invocation as Running before any metric or heartbeat, and
/// shares every snapshot with the wait path.
async fn announce_watcher(
    config: &WatcherConfig,
    parent_sink: &NatsEventSink,
    shared: &SharedSnapshots,
) -> SubagentProgressReporter {
    let WatchedPrompt {
        agent,
        session_id,
        prompt_id,
        tool_call_id,
        ..
    } = &config.watched;
    SubagentProgressReporter::start_shared(
        ReportedInvocation {
            agent: agent.clone(),
            session_id: session_id.clone(),
            invocation_id: prompt_id.clone(),
            tool_call_id: tool_call_id.clone(),
        },
        Some(parent_sink.clone()),
        config.session_metadata.clone(),
        config.heartbeat,
        Some(std::sync::Arc::clone(shared)),
    )
    .await
}

/// The child's live event feed. The subject is keyed by the child's STORAGE
/// key (`session_key(agent, local_id)`), because that is the identity the
/// child worker's own sink publishes under (its activation carries the
/// storage key); the local session id is a presentation name that never
/// reaches a subject.
///
/// Lossy by contract; on failure the watcher still tracks liveness through
/// the reporter heartbeat and terminal truth through the durable scan.
/// Advisories carry no prompt identity, so events of a subsequently
/// dispatched prompt on the same child can be counted into this watcher's
/// running row until its next durable read (bounded by the poll interval).
/// On settlement the durable `TurnEnd` total supersedes the usage estimate
/// for a COMPLETED turn only; a Failed or Cancelled turn keeps the lossy
/// accumulated estimate, and the tool-call count and elapsed time may also
/// still include the following prompt's advisories. Carrying prompt
/// identity (an invocation id) in the advisory contract is the follow-up
/// that would make the live view exact.
async fn subscribe_child_events(config: &WatcherConfig) -> Option<async_nats::Subscriber> {
    match config
        .client
        .subscribe(events_subject(&config.storage))
        .await
    {
        Ok(subscriber) => Some(subscriber),
        Err(error) => {
            log::debug!(
                "sub-agent progress watcher cannot follow live events of '{}': {error}",
                config.watched.session_id
            );
            None
        }
    }
}

/// Everything the watch loop owns, so the loop is a function of one bundle
/// instead of a spread of arguments.
struct Watching {
    config: WatcherConfig,
    scan: WatchScan,
    reporter: SubagentProgressReporter,
    subscriber: Option<async_nats::Subscriber>,
    parent_sink: NatsEventSink,
    shared: SharedSnapshots,
    /// Consecutive refresh failures. The first logs a warning; the rest log
    /// at debug so a long broker outage is visible but does not spam.
    refresh_failures: u32,
}

async fn watch_until_settled(mut watcher: Watching) {
    let mut poll = tokio::time::interval(watcher.config.poll);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let next_child_event = async {
            match watcher.subscriber.as_mut() {
                Some(subscriber) => subscriber.next().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            message = next_child_event => {
                let Some(message) = message else {
                    // The broker closed the advisory stream (connection loss
                    // or server restart). Heartbeats and durable polling keep
                    // the row accurate; resubscribing here would race the
                    // client's own reconnect logic for no additional truth.
                    log::warn!(
                        "sub-agent progress watcher lost the live event stream of '{}'; \
                         continuing from heartbeats and durable polling",
                        watcher.config.watched.session_id
                    );
                    watcher.subscriber = None;
                    continue;
                };
                if let Ok(envelope) = AdvisoryEnvelope::from_bytes(&message.payload) {
                    // The reporter's sink keeps only this child's direct
                    // usage and tool starts, ignoring nested delegations
                    // exactly like the synchronous path.
                    watcher.reporter.sink().emit(envelope.event);
                }
            }
            _ = poll.tick() => match watcher.scan.refresh().await {
                Ok(WatchState::Terminal { status, usage }) => {
                    watcher.settle(status, usage).await;
                    return;
                }
                Ok(WatchState::EvidenceError(evidence)) => {
                    // Same rule as the observer: once the bounded scan
                    // cannot prove the prompt's history, no later poll
                    // repairs that. Keep the row's last live view.
                    log::warn!(
                        "stop sub-agent progress watcher: prompt '{}' of '{}' \
                         has a permanent evidence gap ({evidence})",
                        watcher.config.watched.prompt_id, watcher.config.watched.session_id
                    );
                    return;
                }
                Ok(WatchState::Pending) => watcher.refresh_failures = 0,
                Err(stop @ (Stop::Expired | Stop::Unavailable)) => {
                    watcher.note_refresh_failure(stop);
                }
                // Unreachable today: the per-pass budget has a fresh,
                // never-cancelled token and no caller stops, so it can't
                // yield Cancelled, NewMessage or CallerDeadline. Warn if
                // that ever changes, since the row would freeze Running.
                Err(stop) => {
                    log::warn!(
                        "stop durable refresh for dispatched prompt '{}' of '{}': {stop:?}",
                        watcher.config.watched.prompt_id, watcher.config.watched.session_id
                    );
                    return;
                }
            },
        }
    }
}

impl Watching {
    /// Durable settlement: the reporter's own terminal publish, one retry if
    /// its actor stopped first, and the finished-snapshot retention that
    /// keeps the measured view for later same-process waits.
    async fn settle(&mut self, status: SessionPromptStatus, usage: Option<CompletionTokenUsage>) {
        let watched = &self.config.watched;
        // Replace the lossy advisory estimate with the durable turn total
        // before the terminal snapshot.
        if let Some(usage) = usage {
            self.reporter.set_usage(usage);
        }
        if let Err(error) = self.reporter.finish(progress_status(status)).await {
            log::warn!(
                "publish terminal progress for dispatched prompt '{}' failed: {error:#}",
                watched.prompt_id
            );
            retry_terminal_publish(&self.parent_sink, &self.shared).await;
        }
        if let Some(snapshot) = self.shared.lock().expect("watcher share").clone() {
            retain_finished(
                &self.config.watched.parent,
                &self.config.storage,
                &watched.prompt_id,
                snapshot,
            );
        }
    }

    /// A transient broker failure or a pass that ran out of its I/O bound:
    /// back off to the next tick, keeping the cursor. Never silent, never
    /// spammy.
    fn note_refresh_failure(&mut self, stop: Stop) {
        let watched = &self.config.watched;
        self.refresh_failures += 1;
        if self.refresh_failures == 1 {
            log::warn!(
                "durable refresh for dispatched prompt '{}' of '{}' \
                 failed ({stop:?}); retrying on the next poll",
                watched.prompt_id,
                watched.session_id
            );
        } else {
            log::debug!(
                "durable refresh for dispatched prompt '{}' of '{}' \
                 still failing ({stop:?}, attempt {})",
                watched.prompt_id,
                watched.session_id,
                self.refresh_failures
            );
        }
    }
}

/// Bind the durable scan before anything is announced. `Ok(None)` when the
/// prompt binding is gone; `Err(())` when broker reads kept failing for
/// several poll intervals.
async fn bind_scan(config: &WatcherConfig) -> Result<Option<WatchScan>, ()> {
    let mut remaining = BIND_ATTEMPTS;
    loop {
        match WatchScan::bind(WatchTarget {
            metadata: &config.session_metadata,
            jetstream: &config.jetstream,
            storage: &config.storage,
            prompt_id: &config.watched.prompt_id,
            pass_limit: config.pass_limit,
        })
        .await
        {
            Ok(scan) => return Ok(Some(scan)),
            Err(WatchBindError::UnknownPrompt) => {
                log::debug!(
                    "stop sub-agent progress watcher: prompt '{}' in '{}' is no longer bound",
                    config.watched.prompt_id,
                    config.storage
                );
                return Ok(None);
            }
            Err(WatchBindError::Unavailable) => {
                log::debug!(
                    "initial durable refresh for dispatched prompt '{}' of '{}' failed; retrying binding",
                    config.watched.prompt_id, config.storage
                );
                remaining -= 1;
                if remaining == 0 {
                    log::warn!(
                        "stop sub-agent progress watcher: cannot read the binding of prompt \
                         '{}' in '{}'",
                        config.watched.prompt_id,
                        config.storage
                    );
                    return Err(());
                }
                tokio::time::sleep(config.poll).await;
            }
        }
    }
}

/// Terminal snapshot for a prompt that settled before this watcher bound it.
/// An earlier watcher of the same prompt may have retained its measured view
/// (prefilled into `shared` by `ensure_watcher`), so the live fields carry
/// over; with no measured view the durable classification and the `TurnEnd`
/// total are the whole truth. Publishing is required-delivery + flush, same
/// as the reporter's own finish.
async fn publish_settled_progress(
    watched: &WatchedPrompt,
    parent_sink: &NatsEventSink,
    terminal: WatchState,
    shared: &SharedSnapshots,
) {
    let WatchState::Terminal { status, usage } = terminal else {
        return;
    };
    let storage =
        harnx_core::session_identity::session_key(Some(&watched.agent), &watched.session_id);
    let live = shared.lock().expect("watcher share").clone();
    let mut snapshot = terminal_progress_value(
        &ChildIdentity {
            agent: watched.agent.clone(),
            session_id: watched.session_id.clone(),
            prompt_id: watched.prompt_id.clone(),
        },
        status,
        usage,
        live,
    );
    // The advisory labels the parent transcript's call; the replay summary's
    // id is filled by `completed_subagent_progress`.
    snapshot.tool_call_id = watched.tool_call_id.clone();
    *shared.lock().expect("watcher share") = Some(snapshot.clone());
    retain_finished(
        &watched.parent,
        &storage,
        &watched.prompt_id,
        snapshot.clone(),
    );
    parent_sink.emit_required(progress_event(&snapshot));
    if let Err(error) = parent_sink.flush().await {
        log::warn!(
            "publish terminal progress for settled prompt '{}' of '{}': {error:#}",
            watched.prompt_id,
            watched.session_id
        );
    }
}

/// One best-effort retry after the reporter's terminal publish failed. The
/// reporter actor stops with its `Finish` command, so the watcher re-emits
/// the terminal snapshot itself; only an already-terminal snapshot is
/// republished — if the actor died before finishing, the share still holds a
/// `Running` view and re-sending it would resurrect a dead row.
pub(super) async fn retry_terminal_publish(parent_sink: &NatsEventSink, shared: &SharedSnapshots) {
    let snapshot = shared
        .lock()
        .expect("watcher share")
        .clone()
        .filter(|snapshot| snapshot.status != SubAgentProgressStatus::Running);
    let Some(snapshot) = snapshot else {
        return;
    };
    parent_sink.emit_required(progress_event(&snapshot));
    let _ = parent_sink.flush().await;
}

/// Terminal progress status from the prompt's durable classification. A
/// timed-out child did not finish its work, so it reports as failed; an
/// explicit cancel keeps its own status. Non-terminal states never finish a
/// watcher, so their mapping only exists to satisfy exhaustiveness.
pub(super) fn progress_status(status: SessionPromptStatus) -> SubAgentProgressStatus {
    match status {
        SessionPromptStatus::Completed => SubAgentProgressStatus::Done,
        SessionPromptStatus::Failed | SessionPromptStatus::TimedOut => {
            SubAgentProgressStatus::Failed
        }
        SessionPromptStatus::Cancelled => SubAgentProgressStatus::Cancelled,
        SessionPromptStatus::Pending
        | SessionPromptStatus::Blocked
        | SessionPromptStatus::Unknown => SubAgentProgressStatus::Running,
    }
}

fn progress_event(snapshot: &SubAgentProgress) -> AgentEvent {
    AgentEvent::sub_agent(
        AgentSource {
            agent: snapshot.agent.clone(),
            session_id: Some(snapshot.session_id.clone()),
            model: None,
        },
        AgentEvent::Turn(TurnEvent::SubAgentProgress(snapshot.clone())),
    )
}

/// Durable terminal progress summary attached to a settled observation's tool
/// result, so `completed_subagent_progress` (nats_session/completion.rs)
/// recovers the child's row when live advisories were lost. Only terminal
/// observations carry it: a dispatch receipt and an expired wait never do,
/// so replay cannot misreport either as completion.
///
/// The summary starts from the live watcher's last published snapshot, so the
/// durable field cannot win a first-terminal-wins race against a sparser live
/// view: title, tool count and accumulated usage carry over, and usage
/// upgrades to the durable `TurnEnd` total when one was recorded (a
/// `Failed`/`Cancelled` terminal has none, keeping the live estimate instead
/// of reporting zero). `elapsed_ms` is the child's runtime as the watcher
/// tracked it — never the observation's wait duration; with no watcher state
/// (a parent that restarted before settlement) it is zero.
pub(super) fn terminal_progress_value(
    child: &ChildIdentity,
    status: SessionPromptStatus,
    usage: Option<CompletionTokenUsage>,
    live: Option<SubAgentProgress>,
) -> SubAgentProgress {
    let live = live.unwrap_or_else(|| SubAgentProgress {
        invocation_id: child.prompt_id.clone(),
        agent: child.agent.clone(),
        session_id: child.session_id.clone(),
        status: SubAgentProgressStatus::Running,
        elapsed_ms: 0,
        usage: CompletionTokenUsage::default(),
        tool_call_count: 0,
        title: None,
        tool_call_id: None,
    });
    SubAgentProgress {
        invocation_id: child.prompt_id.clone(),
        agent: child.agent.clone(),
        session_id: child.session_id.clone(),
        status: progress_status(status),
        elapsed_ms: live.elapsed_ms,
        usage: usage.unwrap_or(live.usage),
        tool_call_count: live.tool_call_count,
        title: live.title,
        // The result's own id names the call on replay
        // (`completed_subagent_progress` fills it in).
        tool_call_id: None,
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_statuses_map_to_progress_states() {
        // A timed-out child did not finish its work, so it reports Failed.
        let cases = [
            (SessionPromptStatus::Completed, SubAgentProgressStatus::Done),
            (SessionPromptStatus::Failed, SubAgentProgressStatus::Failed),
            (
                SessionPromptStatus::TimedOut,
                SubAgentProgressStatus::Failed,
            ),
            (
                SessionPromptStatus::Cancelled,
                SubAgentProgressStatus::Cancelled,
            ),
        ];
        for (status, expected) in cases {
            assert_eq!(progress_status(status), expected, "{status:?}");
        }
    }

    #[test]
    fn watcher_keys_separate_prompts_and_children() {
        let first = watcher_key("parent", "metis/child", "prompt-1");
        assert_eq!(first, watcher_key("parent", "metis/child", "prompt-1"));
        // One watcher per child invocation, and per parent observing it.
        let distinct = [
            watcher_key("parent", "metis/child", "prompt-2"),
            watcher_key("parent", "metis/other", "prompt-1"),
            watcher_key("parent-2", "metis/child", "prompt-1"),
        ];
        for other in distinct {
            assert_ne!(first, other);
        }
    }

    #[test]
    fn registry_guard_removes_only_its_own_entry() {
        let key = watcher_key("parent", "metis/child", "prompt-1");
        let stale: SharedSnapshots = std::sync::Arc::new(Mutex::new(None));
        let fresh: SharedSnapshots = std::sync::Arc::new(Mutex::new(None));
        let registry = active_watchers();
        // The guard's watcher is still the registered one: entry removed.
        registry
            .lock()
            .expect("watcher registry")
            .insert(key.clone(), std::sync::Arc::clone(&stale));
        let guard = RegistryGuard {
            key: key.clone(),
            shared: std::sync::Arc::downgrade(&stale),
        };
        drop(guard);
        assert!(
            !registry
                .lock()
                .expect("watcher registry")
                .contains_key(&key),
            "the guard removes its own watcher's entry"
        );
        // A replacement registered under the same key stays when the old
        // watcher's guard drops (panic path).
        registry
            .lock()
            .expect("watcher registry")
            .insert(key.clone(), std::sync::Arc::clone(&fresh));
        let stale_guard = RegistryGuard {
            key: key.clone(),
            shared: std::sync::Arc::downgrade(&stale),
        };
        drop(stale_guard);
        let current = registry
            .lock()
            .expect("watcher registry")
            .get(&key)
            .cloned();
        assert!(
            current.is_some_and(|current| std::sync::Arc::ptr_eq(&current, &fresh)),
            "the guard does not remove a newer watcher registered under the same key"
        );
        registry.lock().expect("watcher registry").remove(&key);
    }

    #[test]
    fn finished_snapshot_cache_is_keyed_by_parent_storage_and_prompt() {
        // A local cache: the eviction test's 129 inserts must not race this
        // test's lookups through the process-global one. The key is
        // `watcher_key` — parent scope, child storage, prompt — so unrelated
        // parent scopes never see each other's retained snapshots.
        let mut cache = FinishedSnapshots::empty();
        retain_finished_in(
            &mut cache,
            watcher_key("parent", "storage-a", "prompt-1"),
            terminal_snapshot(),
        );
        assert!(
            cache
                .finished(&watcher_key("parent", "storage-a", "prompt-1"))
                .is_some(),
            "the retained snapshot is found under its own key"
        );
        assert!(
            cache
                .finished(&watcher_key("parent", "storage-a", "prompt-2"))
                .is_none(),
            "another prompt of the same child does not collide"
        );
        assert!(
            cache
                .finished(&watcher_key("parent", "storage-b", "prompt-1"))
                .is_none(),
            "another child's storage does not collide"
        );
        assert!(
            cache
                .finished(&watcher_key("parent-2", "storage-a", "prompt-1"))
                .is_none(),
            "another parent scope's wait does not collide"
        );
    }

    #[test]
    fn finished_snapshot_cache_evicts_in_fifo_order() {
        let mut cache = FinishedSnapshots::empty();
        for index in 0..(FINISHED_SNAPSHOT_LIMIT + 1) {
            retain_finished_in(
                &mut cache,
                watcher_key("parent", &format!("storage-{index}"), "prompt"),
                terminal_snapshot(),
            );
        }
        assert!(
            cache
                .finished(&watcher_key("parent", "storage-0", "prompt"))
                .is_none(),
            "the oldest entry is evicted once the bound is reached"
        );
        assert!(
            cache
                .finished(&watcher_key(
                    "parent",
                    &format!("storage-{FINISHED_SNAPSHOT_LIMIT}"),
                    "prompt"
                ))
                .is_some(),
            "the newest entry survives the eviction"
        );
    }

    #[test]
    fn running_snapshots_are_never_retained() {
        let mut cache = FinishedSnapshots::empty();
        retain_finished_in(
            &mut cache,
            watcher_key("parent", "storage-running", "prompt"),
            SubAgentProgress {
                status: SubAgentProgressStatus::Running,
                ..terminal_snapshot()
            },
        );
        assert!(
            cache
                .finished(&watcher_key("parent", "storage-running", "prompt"))
                .is_none(),
            "a Running snapshot must not enter the finished cache: a later \
             wait would prefill and resurrect a row its watcher abandoned"
        );
    }

    #[test]
    fn retained_terminal_flags_only_a_terminal_share() {
        let shared: SharedSnapshots = std::sync::Arc::new(Mutex::new(None));
        assert!(
            !retained_terminal(&shared),
            "a fresh cell has no settled view to preserve"
        );
        *shared.lock().expect("watcher share") = Some(live_defaults());
        assert!(
            !retained_terminal(&shared),
            "a live Running share announces normally"
        );
        *shared.lock().expect("watcher share") = Some(terminal_snapshot());
        assert!(
            retained_terminal(&shared),
            "a retained terminal snapshot suppresses the Running announcement"
        );
    }

    /// One snapshot from the shared test identity, varying only the measured
    /// fields — the base `live_defaults` and `terminal_snapshot` build on it.
    fn snapshot(
        status: SubAgentProgressStatus,
        elapsed_ms: u64,
        tool_call_count: u64,
        title: Option<&str>,
    ) -> SubAgentProgress {
        SubAgentProgress {
            invocation_id: "prompt-1".into(),
            agent: "metis".into(),
            session_id: "child-1".into(),
            status,
            elapsed_ms,
            usage: CompletionTokenUsage::default(),
            tool_call_count,
            title: title.map(str::to_owned),
            tool_call_id: None,
        }
    }

    fn live_defaults() -> SubAgentProgress {
        snapshot(SubAgentProgressStatus::Running, 0, 0, None)
    }

    fn terminal_snapshot() -> SubAgentProgress {
        snapshot(SubAgentProgressStatus::Done, 4_200, 2, Some("measured"))
    }

    /// The measured fields one settled summary must carry, compared against
    /// an expected view built with `snapshot` — one assertion instead of a
    /// block per case.
    fn assert_measured_view(summary: &SubAgentProgress, expected: &SubAgentProgress) {
        assert_eq!(summary.status, expected.status);
        assert_eq!(
            summary.elapsed_ms, expected.elapsed_ms,
            "child runtime, not wait duration"
        );
        assert_eq!(
            summary.usage, expected.usage,
            "the durable TurnEnd total wins when one was recorded"
        );
        assert_eq!(summary.tool_call_count, expected.tool_call_count);
        assert_eq!(summary.title, expected.title);
    }

    #[test]
    fn terminal_summary_merges_live_and_durable_state() {
        let live = SubAgentProgress {
            usage: usage(120, 45, 10),
            tool_call_count: 7,
            title: Some("child work".into()),
            tool_call_id: Some("parent-call-1".into()),
            elapsed_ms: 4_200,
            ..live_defaults()
        };
        let durable = usage(200, 60, 12);
        let summary = terminal_progress_value(
            &child_identity(),
            SessionPromptStatus::Completed,
            Some(durable.clone()),
            Some(live),
        );
        let mut expected = snapshot(SubAgentProgressStatus::Done, 4_200, 7, Some("child work"));
        expected.usage = durable;
        assert_measured_view(&summary, &expected);
        assert_eq!(
            summary.tool_call_id, None,
            "replay fills the call id itself"
        );

        // No durable TurnEnd (Failed/Cancelled): the live estimate survives
        // instead of collapsing to zero.
        let summary = terminal_progress_value(
            &child_identity(),
            SessionPromptStatus::Cancelled,
            None,
            Some(SubAgentProgress {
                usage: usage(90, 30, 0),
                ..live_defaults()
            }),
        );
        let mut expected = snapshot(SubAgentProgressStatus::Cancelled, 0, 0, None);
        expected.usage = usage(90, 30, 0);
        assert_measured_view(&summary, &expected);

        // No watcher state at all: sparse but correct, never a wait duration.
        let summary =
            terminal_progress_value(&child_identity(), SessionPromptStatus::Failed, None, None);
        assert_measured_view(
            &summary,
            &snapshot(SubAgentProgressStatus::Failed, 0, 0, None),
        );
    }

    fn usage(input: u64, output: u64, cached: u64) -> CompletionTokenUsage {
        CompletionTokenUsage::new(Some(input), Some(output), Some(cached))
    }

    fn child_identity() -> ChildIdentity {
        ChildIdentity {
            agent: "metis".into(),
            session_id: "child-1".into(),
            prompt_id: "prompt-1".into(),
        }
    }
}
