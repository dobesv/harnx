use crate::agent_loop::AgentLoopContext;
use crate::config::HARNX_NATS_URL_ENV;
use crate::config::{Config, GlobalConfig, Input};
use crate::nats_tool_provider::{NatsInFlightCalls, NatsToolProvider, RegistryRevision};
use crate::tool::{CompletionText, PrepareHandoffFn};
use crate::tool_registry_watch::registry_epoch;
use crate::utils::AbortSignal;
use harnx_core::instance::ServerScope;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Shared runtime state for one tool-evaluation round.
pub struct ToolRoundParams<'a> {
    pub config: &'a GlobalConfig,
    pub instance_id: &'a ServerScope,
    pub input: &'a Input,
    pub completion: CompletionText<'a>,
    pub abort_signal: &'a AbortSignal,
    pub working_dir: Option<&'a Path>,
    pub prepare_handoff_fn: Option<Arc<PrepareHandoffFn>>,

    pub pending_async_context: Option<Arc<Mutex<Option<String>>>>,
    pub tool_loop_guard: Option<crate::tool_loop_guard::ToolLoopGuardHandle>,
}

/// Inputs used to assemble the provider and rendering context for a tool round.
pub struct BuildToolEvalContextParams<'a> {
    pub config: &'a GlobalConfig,
    pub instance_id: &'a ServerScope,
    pub agent_use_tools: Option<&'a str>,
    pub current_agent_package: Option<String>,
    pub working_dir: Option<&'a Path>,
    pub prepare_handoff_fn: Option<Arc<PrepareHandoffFn>>,

    pub pending_async_context: Option<Arc<Mutex<Option<String>>>>,
    /// Round context handoff for the script-mode `bash` provider. The
    /// caller fills the slot with the built context wrapped in an `Arc` right
    /// after construction; `None` means `bash` fails closed on this path.
    pub bash_context_slot: Option<Arc<crate::bash_provider::BashEvalContextSlot>>,
    /// Command-scoped `bash` provider inputs for one operator script call
    /// (#2630): active even with an empty inner catalog, command-resolved
    /// declarations, operator-only inner consent. `None` keeps the ordinary
    /// config-driven provider.
    pub command_bash: Option<crate::bash_provider::CommandBashInputs>,
    /// Additional base providers appended after the built-in ones, ahead of
    /// the `bash` and script-tool wrappers, so inner calls from both can
    /// dispatch to them. Embedders and integration tests use this to inject
    /// in-process tools; ordinary rounds leave it empty.
    pub extra_base_providers: Vec<Arc<dyn harnx_core::tool::ToolProvider>>,
}

impl<'a> BuildToolEvalContextParams<'a> {
    pub fn new(config: &'a GlobalConfig, instance_id: &'a ServerScope) -> Self {
        Self {
            config,
            instance_id,
            agent_use_tools: None,
            current_agent_package: None,
            working_dir: None,
            prepare_handoff_fn: None,

            pending_async_context: None,
            bash_context_slot: None,
            command_bash: None,
            extra_base_providers: Vec::new(),
        }
    }

    pub fn with_agent_use_tools(mut self, agent_use_tools: Option<&'a str>) -> Self {
        self.agent_use_tools = agent_use_tools;
        self
    }

    pub fn with_current_agent_package(mut self, package: Option<String>) -> Self {
        self.current_agent_package = package;
        self
    }

    pub fn with_command_bash(mut self, inputs: crate::bash_provider::CommandBashInputs) -> Self {
        self.command_bash = Some(inputs);
        self
    }
}

impl AgentLoopContext {
    pub(crate) fn tool_round_params<'a>(
        &'a self,
        config: &'a GlobalConfig,
        input: &'a Input,
        completion: CompletionText<'a>,
    ) -> ToolRoundParams<'a> {
        ToolRoundParams {
            config,
            instance_id: &self.instance_id,
            input,
            completion,
            abort_signal: &self.abort_signal,
            working_dir: self.working_dir.as_deref(),
            prepare_handoff_fn: self.prepare_handoff_fn.clone(),

            pending_async_context: self.pending_async_context.clone(),
            tool_loop_guard: Some(self.tool_loop_guard.clone()),
        }
    }
}

const REGISTRATION_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const DISCOVERY_CACHE_MAX_ENTRIES: usize = 256;

/// `NatsToolProvider` captures the invoking session, so registration
/// snapshots may only be shared within that exact context.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ToolDiscoveryKey {
    instance_id: ServerScope,
    active_package: Option<String>,
    parent_session_id: Option<String>,
    session_owner: Option<String>,
}
type ToolDiscoveryCache =
    std::sync::Mutex<HashMap<ToolDiscoveryKey, CachedDiscovery<NatsToolProvider>>>;
static NATS_TOOL_DISCOVERY_CACHE: OnceLock<ToolDiscoveryCache> = OnceLock::new();

fn tool_discovery_key(
    config: &Config,
    instance_id: &ServerScope,
    active_package: Option<&str>,
) -> ToolDiscoveryKey {
    ToolDiscoveryKey {
        instance_id: instance_id.clone(),
        active_package: active_package.map(str::to_string),
        parent_session_id: config.session.as_ref().map(|session| session.storage_key()),
        // The snapshot carries the owner's user-scoped registrations, so it
        // may only be shared with discoveries for the same identity.
        session_owner: config
            .extract_agent()
            .user_principals()
            .and_then(|principals| principals.user_id.clone()),
    }
}

struct CachedDiscovery<T> {
    provider: Option<Arc<T>>,
    discovered_at: Instant,
    /// The registry epoch the snapshot was taken under. The registry watch
    /// (decision 5b) bumps the global epoch on every bucket change, so an
    /// entry from an older epoch is stale no matter how fresh its TTL is.
    epoch: u64,
    /// The registry bucket's stream sequence when the snapshot *started* its
    /// scan. Every KV write and delete moves the sequence at the moment the
    /// broker acks it, so comparing against the sequence the next completion
    /// reads is the deterministic half of decision 5b: no event delivery, no
    /// timing dependence on the async watch.
    ///
    /// `None` when the scan's pre-scan revision read failed: no proof exists,
    /// and caching a fabricated 0 would let a later bucket deletion probe
    /// `Some(0)`, match, and serve removed tools until the TTL.
    revision: Option<u64>,
}

// Only the tests below use the epoch-only forms; production goes through
// `cached_discovery_snapshot` plus `snapshot_is_current`.
#[cfg(test)]
fn cached_discovery<K: Eq + std::hash::Hash, T>(
    cache: &std::sync::Mutex<HashMap<K, CachedDiscovery<T>>>,
    key: &K,
    now: Instant,
    ttl: Duration,
    epoch: u64,
) -> Option<Option<Arc<T>>> {
    cached_discovery_matching(cache, key, now, ttl, |entry| entry.epoch == epoch)
}

/// [`cached_discovery`] with a caller-supplied freshness predicate for tests.
#[cfg(test)]
fn cached_discovery_matching<K: Eq + std::hash::Hash, T>(
    cache: &std::sync::Mutex<HashMap<K, CachedDiscovery<T>>>,
    key: &K,
    now: Instant,
    ttl: Duration,
    is_current: impl Fn(&CachedDiscovery<T>) -> bool,
) -> Option<Option<Arc<T>>> {
    let mut cache = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    cache.retain(|_, entry| now.saturating_duration_since(entry.discovered_at) < ttl);
    cache
        .get(key)
        .filter(|entry| is_current(entry))
        .map(|entry| entry.provider.clone())
}

/// The live cache entry's snapshot (when it holds one) plus the epoch and
/// registry revision it was discovered under, for
/// [`snapshot_is_current`] to re-check after probing the current revision
/// through the snapshot's own connection.
fn cached_discovery_snapshot<K: Eq + std::hash::Hash, T>(
    cache: &std::sync::Mutex<HashMap<K, CachedDiscovery<T>>>,
    key: &K,
    now: Instant,
    ttl: Duration,
) -> Option<CachedSnapshot<T>> {
    let mut cache = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    cache.retain(|_, entry| now.saturating_duration_since(entry.discovered_at) < ttl);
    cache.get(key).and_then(|entry| {
        entry.provider.clone().map(|provider| CachedSnapshot {
            provider,
            epoch: entry.epoch,
            revision: entry.revision,
        })
    })
}

#[cfg(test)]
fn cache_discovery<K: Clone + Eq + std::hash::Hash, T>(
    cache: &std::sync::Mutex<HashMap<K, CachedDiscovery<T>>>,
    key: K,
    provider: Option<Arc<T>>,
    discovered_at: Instant,
    epoch: u64,
) {
    cache_discovery_with_revision(
        cache,
        key,
        CachedDiscovery {
            provider,
            discovered_at,
            epoch,
            revision: None,
        },
    )
}

fn cache_discovery_with_revision<K: Clone + Eq + std::hash::Hash, T>(
    cache: &std::sync::Mutex<HashMap<K, CachedDiscovery<T>>>,
    key: K,
    entry: CachedDiscovery<T>,
) {
    let mut cache = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let discovered_at = entry.discovered_at;
    if cache
        .get(&key)
        .is_some_and(|current| current.discovered_at > discovered_at)
    {
        return;
    }
    // Session-scoped keys may never be read again after completion, so writes
    // must prune stale entries too. The capacity also bounds short-lived bursts;
    // refreshing an existing session never evicts another provider.
    cache.retain(|_, current| {
        discovered_at.saturating_duration_since(current.discovered_at)
            < REGISTRATION_REFRESH_INTERVAL
    });
    if !cache.contains_key(&key) && cache.len() >= DISCOVERY_CACHE_MAX_ENTRIES {
        let oldest_key = cache
            .iter()
            .min_by_key(|(_, current)| current.discovered_at)
            .map(|(key, _)| key.clone());
        if let Some(oldest_key) = oldest_key {
            cache.remove(&oldest_key);
        }
    }
    cache.insert(key, entry);
}

pub async fn discover_nats_tool_provider_cached(
    config: &Config,
    instance_id: &ServerScope,
    active_package: Option<&str>,
) -> Option<Arc<NatsToolProvider>> {
    // Decision 5b: a registration written or deleted under this scope must be
    // visible to the very next completion, without waiting out the 30 s cache
    // interval. The watch keeps an epoch for eager invalidation, but the
    // deterministic guarantee is the revision check below: every KV write and
    // delete moves the registry stream's sequence at the moment the broker
    // acks it, so comparing against the sequence this snapshot started
    // scanning under cannot miss a change, no matter when the watch delivers.
    crate::tool_registry_watch::ensure_registry_watch(config).await;
    let cache = NATS_TOOL_DISCOVERY_CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let key = tool_discovery_key(config, instance_id, active_package);
    let snapshot =
        cached_discovery_snapshot(cache, &key, Instant::now(), REGISTRATION_REFRESH_INTERVAL);
    match snapshot_if_current(snapshot).await {
        Some(provider) => Some(provider),
        None => discover_nats_tool_provider_fresh(config, instance_id, active_package).await,
    }
}

/// A live cache entry plus the registry freshness evidence it was stored with.
struct CachedSnapshot<T> {
    provider: Arc<T>,
    epoch: u64,
    revision: Option<u64>,
}

/// Re-prove one cached snapshot's freshness (decision 5b).
///
/// Probes the current registry revision through the snapshot's own
/// connection. Opening a fresh connection per probe (`Config::nats_client`
/// has no connection pool) cost hundreds of milliseconds on CI and multiplied
/// through every session start and tool dispatch, until delegated children
/// missed their inherited deadline (PR #2835 macOS CI failure). Only a cache
/// miss pays for a connection.
async fn snapshot_if_current(
    snapshot: Option<CachedSnapshot<NatsToolProvider>>,
) -> Option<Arc<NatsToolProvider>> {
    let snapshot = snapshot?;
    let epoch = registry_epoch();
    let revision = snapshot.provider.registry_revision_probe().await;
    snapshot_is_current(&snapshot, epoch, revision).then_some(snapshot.provider)
}

/// Decision 5b freshness for one cached snapshot.
///
/// `revision` is the registry stream's sequence as of now. When both sides
/// carry revision evidence, the watch epoch and the sequence must both
/// match, so a deleted bucket (sequence 0) invalidates a snapshot cached
/// with any later revision. When either side lacks evidence — the probe
/// could not reach the broker, or the scan's pre-scan read failed — only the
/// watch epoch can vouch for the snapshot, the pre-change best-effort
/// behavior; a snapshot cached without proof is additionally invalidated by
/// any successful probe, so one rescan captures a real sequence.
fn snapshot_is_current<T>(snapshot: &CachedSnapshot<T>, epoch: u64, revision: Option<u64>) -> bool {
    match (snapshot.revision, revision) {
        (Some(cached), Some(current)) => snapshot.epoch == epoch && cached == current,
        // The snapshot carries no revision proof (its pre-scan read failed):
        // any reachable probe invalidates it, forcing one rescan that can
        // capture a real sequence — a fabricated 0 here would let a bucket
        // deletion match and serve removed tools until the TTL.
        (None, Some(_)) => false,
        // The probe could not reach the broker: the watch epoch alone vouches
        // for the snapshot — the pre-change best-effort behavior.
        (_, None) => snapshot.epoch == epoch,
    }
}

/// Refresh tool discovery immediately and replace the cached snapshot.
///
/// Worker activations do not call this (they use
/// [`discover_nats_tool_provider_fresh_forced`]); callers here can tolerate
/// the failure window below.
///
/// A failure holds this scope's [`FAILED_DISCOVERY_RETRY_INTERVAL`] window
/// shut, so a sustained outage costs roughly one connect attempt (plus one
/// warn) per interval per scope instead of one per completion — best-effort
/// under concurrent callers, since the window is checked, not held. Recovery
/// stays as fast as the window: no negative entry is cached, and the first
/// attempt after expiry is a real discovery, not a stale snapshot.
pub(crate) async fn discover_nats_tool_provider_fresh(
    config: &Config,
    instance_id: &ServerScope,
    active_package: Option<&str>,
) -> Option<Arc<NatsToolProvider>> {
    let cache = NATS_TOOL_DISCOVERY_CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let key = tool_discovery_key(config, instance_id, active_package);
    if failed_discovery_window_holds(&key, Instant::now()) {
        log::debug!(
            "NATS tool discovery attempt suppressed: recent attempt failed under scope '{}'",
            instance_id.as_str()
        );
        return None;
    }
    discover_fresh_ungated(config, instance_id, &key, cache).await
}

/// [`discover_nats_tool_provider_fresh`], bypassing the failure window.
///
/// Activation calls this after waiting for this session's on-demand tool
/// servers to register: their registrations must be visible on the first
/// model request (decision 5b), not after the retry interval. A success here
/// reopens the window for the cached path in the same scope.
pub(crate) async fn discover_nats_tool_provider_fresh_forced(
    config: &Config,
    instance_id: &ServerScope,
    active_package: Option<&str>,
) -> Option<Arc<NatsToolProvider>> {
    let cache = NATS_TOOL_DISCOVERY_CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let key = tool_discovery_key(config, instance_id, active_package);
    discover_fresh_ungated(config, instance_id, &key, cache).await
}

async fn discover_fresh_ungated(
    config: &Config,
    instance_id: &ServerScope,
    key: &ToolDiscoveryKey,
    cache: &ToolDiscoveryCache,
) -> Option<Arc<NatsToolProvider>> {
    // One connection serves both the revision read and the scan:
    // `discover_with_registry_revision` reads the registry sequence on the
    // discovery transport *before* the scan starts (decision 5b). A write
    // the broker acked before that read is in both the scan and the
    // sequence; a write acked after it bumps the sequence past the stored
    // value, so the next completion's comparison catches it and rescans.
    // Reading it after the scan would hide a write that landed mid-scan and
    // was missed by it.
    let discovered = NatsToolProvider::discover_with_registry_revision(
        config,
        instance_id.clone(),
        NatsInFlightCalls::for_instance(instance_id),
        key.active_package.as_deref(),
    )
    .await;
    match discovered {
        Ok((provider, evidence)) => {
            note_discovery_recovered(key);
            Some(cache_discovered_snapshot(cache, key, provider, evidence))
        }
        Err(error) => note_failed_discovery(key.clone(), instance_id, error),
    }
}

/// Store the scan's snapshot under its freshness evidence and return it.
fn cache_discovered_snapshot(
    cache: &ToolDiscoveryCache,
    key: &ToolDiscoveryKey,
    provider: NatsToolProvider,
    evidence: RegistryRevision,
) -> Arc<NatsToolProvider> {
    let provider = Arc::new(provider);
    // An unread revision (its pre-scan read failed) caches no proof rather
    // than a fabricated 0: a later bucket deletion must still invalidate,
    // and the next lookup rescans and captures a real sequence.
    let revision = match evidence {
        RegistryRevision::Sequence(sequence) => Some(sequence),
        RegistryRevision::Unread => None,
    };
    cache_discovery_with_revision(
        cache,
        key.clone(),
        CachedDiscovery {
            provider: Some(provider.clone()),
            discovered_at: Instant::now(),
            epoch: registry_epoch(),
            revision,
        },
    );
    provider
}

/// Record the failure in the scope-keyed retry window and surface it; a
/// connect/subscribe failure here is distinct from "no tools registered"
/// (handled inside `discover` itself) — without this, it produced no
/// provider and no log, the same silent-degradation this cache exists to
/// avoid.
fn note_failed_discovery(
    key: ToolDiscoveryKey,
    instance_id: &ServerScope,
    error: anyhow::Error,
) -> Option<Arc<NatsToolProvider>> {
    record_failed_discovery(key, Instant::now());
    log::warn!(
        "NATS tool discovery failed under scope '{}': {error:#}",
        instance_id.as_str()
    );
    None
}

/// Minimum spacing between failed discovery attempts. Only a sustained
/// outage reaches this; the window's cost is that a just-recovered broker
/// waits out the rest of the interval, at most these 2 seconds.
const FAILED_DISCOVERY_RETRY_INTERVAL: Duration = Duration::from_secs(2);

/// When the last fresh discovery attempt failed, keyed by discovery scope.
/// Keyed, not global: one worker serves many sessions on one broker, and a
/// failure under one scope must not leave another's completions without
/// tools (see [`discover_nats_tool_provider_fresh`]).
static LAST_FAILED_DISCOVERY: OnceLock<std::sync::Mutex<HashMap<ToolDiscoveryKey, Instant>>> =
    OnceLock::new();

fn last_failed_discovery() -> &'static std::sync::Mutex<HashMap<ToolDiscoveryKey, Instant>> {
    LAST_FAILED_DISCOVERY.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

fn failed_discovery_window_holds(key: &ToolDiscoveryKey, now: Instant) -> bool {
    let last = last_failed_discovery()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    last.get(key).is_some_and(|failed_at| {
        now.saturating_duration_since(*failed_at) < FAILED_DISCOVERY_RETRY_INTERVAL
    })
}

fn record_failed_discovery(key: ToolDiscoveryKey, at: Instant) {
    let mut last = last_failed_discovery()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // Session-scoped keys may never be read again; entries older than the
    // window are inert, so prune them here to bound the map.
    last.retain(|_, failed_at| {
        at.saturating_duration_since(*failed_at) < FAILED_DISCOVERY_RETRY_INTERVAL
    });
    last.insert(key, at);
}

fn note_discovery_recovered(key: &ToolDiscoveryKey) {
    last_failed_discovery()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(key);
}

/// Whether this process was handed a broker to discover tool servers on.
///
/// Gate on the broker address, not on `HARNX_SERVER_SCOPE`. The worker creates
/// its instance id in-process and only ever exports it to the children it
/// spawns, so an instance-id check is false in the one process that discovers
/// tools — every turn discovered zero tools and the model saw built-ins only.
///
/// `HARNX_NATS_URL` is set on the worker by its supervisor and inherited by its
/// children, so it marks exactly the processes that already have a broker to
/// talk to. Without it, `resolve_local_nats_server_config` falls back to
/// starting or joining the shared local broker. No process should do that just
/// to look for tools: a broker that has to start recovers its JetStream store
/// before it accepts a connection, which takes seconds once the store has grown.
pub(crate) fn has_broker_address() -> bool {
    std::env::var_os(HARNX_NATS_URL_ENV).is_some()
}

/// Refresh declarations before completion request construction.
pub async fn refresh_nats_tool_declarations(config: &GlobalConfig, instance_id: &ServerScope) {
    if let Some(refresh) =
        has_broker_address().then(|| refresh_nats_tool_declarations_now(config, instance_id))
    {
        refresh.await;
    }
}

// Type-erased box for the same reason as `NatsToolProvider::open_discovery_transport`:
// keeping concrete JetStream futures out of callers' future trees.
fn refresh_nats_tool_declarations_now<'a>(
    config: &'a GlobalConfig,
    instance_id: &'a ServerScope,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
    Box::pin(async move {
        let config_snapshot = config.read().clone();
        let active_package = config_snapshot.active_package();
        let provider = discover_nats_tool_provider_cached(
            &config_snapshot,
            instance_id,
            active_package.as_deref(),
        )
        .await;
        log_discovery_report(provider.as_deref());
        let (declarations, served_names) = declarations_for_all_tools(provider.as_deref());
        *config.read().nats_tool_declarations.write() = declarations;
        // Raw aliases never appear in a declaration list, so the script-tool
        // collision view needs the full route-name set: the model-visible tool
        // list and the round snapshot must agree that such a name is not a
        // script tool's to claim.
        *config.read().nats_served_tool_names.write() = served_names;
    })
}

/// A mismatched scope is silent otherwise: the worker scans one prefix, a
/// typo'd server registers under another, and this turn's declarations just
/// come back empty — indistinguishable from having no tools configured at all.
fn log_discovery_report(provider: Option<&NatsToolProvider>) {
    if let Some(provider) = provider {
        let report = provider.discovery_report();
        if report.found == 0 {
            log::warn!("{}", report.message);
        } else {
            log::debug!("{}", report.message);
        }
    }
}

fn declarations_for_all_tools(
    provider: Option<&NatsToolProvider>,
) -> (
    Vec<harnx_core::tool::ToolDeclaration>,
    std::collections::HashSet<String>,
) {
    provider
        .map(|provider| {
            (
                provider.declarations_for_use_tools(Some("*")),
                provider.route_names().collect(),
            )
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    struct EnvGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvGuard {
        fn unset(key: &'static str) -> Self {
            let previous = std::env::var_os(key);
            // SAFETY: this test holds ENV_LOCK for the guard's lifetime.
            unsafe { std::env::remove_var(key) };
            Self { key, previous }
        }

        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var_os(key);
            // SAFETY: this test holds ENV_LOCK for the guard's lifetime.
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.previous {
                // SAFETY: this test holds ENV_LOCK for the guard's lifetime.
                Some(value) => unsafe { std::env::set_var(self.key, value) },
                // SAFETY: this test holds ENV_LOCK for the guard's lifetime.
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn discovery_cache_separates_agents_with_the_same_local_session_id() {
        let mut config = Config::default();
        let scope = ServerScope::from_string("same-process");
        config.session = Some(harnx_core::session::Session {
            id: "review-12345".into(),
            agent_name: Some("alpha".into()),
            ..Default::default()
        });
        let alpha = tool_discovery_key(&config, &scope, None);
        config.session.as_mut().unwrap().agent_name = Some("beta".into());
        let beta = tool_discovery_key(&config, &scope, None);
        assert_ne!(alpha, beta);
    }

    /// Without a broker address the refresh must return without touching NATS —
    /// otherwise a plain front-end would start a shared server to build a tool list.
    #[test]
    fn refresh_without_broker_address_is_a_no_op() {
        let _lock = ENV_LOCK.lock().expect("env lock");
        let _env = EnvGuard::unset(HARNX_NATS_URL_ENV);
        let config = GlobalConfig::default();

        tokio_test::block_on(async {
            tokio::time::timeout(
                Duration::from_secs(1),
                refresh_nats_tool_declarations(
                    &config,
                    &ServerScope::from_string("non-worker-refresh-test"),
                ),
            )
            .await
            .expect("refresh returns without connecting to NATS");
        });

        assert!(config.read().nats_tool_declarations.read().is_empty());
    }

    /// 4659980786 follow-up: the refresh writes the served-name cache both
    /// ways. A cached provider recording a raw alias hides the script tool
    /// of that name from the model-visible `select_tools` list; a cached
    /// absence clears the route names and the script tool comes back.
    #[tokio::test]
    // The broker-address env var must stay set across the awaited refreshes,
    // and the lock keeps env-mutating tests from interleaving with it.
    #[allow(clippy::await_holding_lock)]
    async fn refresh_hides_provider_served_script_tools_and_clear_reinstates() -> anyhow::Result<()>
    {
        let _lock = ENV_LOCK.lock().expect("env lock");
        let server = crate::nats_test_common::spawn_nats_server()
            .await?
            .ok_or_else(|| anyhow::anyhow!("nats-server required"))?;
        let _env = EnvGuard::set(HARNX_NATS_URL_ENV, server.url());
        let client = async_nats::connect(server.url()).await?;

        let mut config = Config::default();
        let definition: harnx_core::script_tool::ScriptToolDefinition =
            serde_json::from_value(serde_json::json!({
                "name": "read",
                "description": "Wrapper.",
                "parameters": {
                    "type": "object",
                    "properties": {"number": {"type": "integer"}},
                    "required": ["number"],
                    "additionalProperties": false
                },
                "bash": "true"
            }))?;
        config.set_script_tool_definitions(&[definition])?;
        let config = Arc::new(crate::config::ConfigLock::new(config));
        let mut agent_config = harnx_core::agent_config::AgentConfig::from_prompt("test");
        agent_config.set_use_tools(Some(vec!["*".to_string()]));

        // The provider serves the raw alias `read`; its agent-visible name
        // is `fs_read`, which no script tool claims.
        let provider = Arc::new(
            NatsToolProvider::snapshot_test_provider(
                &client,
                vec![("read", "fs", "read")],
                Vec::new(),
                Vec::new(),
            )
            .await?,
        );
        let instance_id = ServerScope::from_string("refresh-served-names-test");
        let cache = NATS_TOOL_DISCOVERY_CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
        let key = tool_discovery_key(
            &config.read(),
            &instance_id,
            config.read().active_package().as_deref(),
        );

        // Cached provider: the refresh records its route names, and the raw
        // alias hides the script tool from the model-visible list. Prime the
        // cache the way a real scan would: this broker has no registry
        // stream, so the snapshot's revision proof is `Some(0)` — the
        // freshness probe reads the same sequence 0 and reuses the entry.
        // The epoch-only helper's `revision: None` would be invalidated by
        // any reachable probe and rediscovered here (a failed pre-scan read
        // must not survive as a cached snapshot).
        cache_discovery_with_revision(
            cache,
            key.clone(),
            CachedDiscovery {
                provider: Some(provider),
                discovered_at: Instant::now(),
                epoch: registry_epoch(),
                revision: Some(0),
            },
        );
        refresh_nats_tool_declarations(&config, &instance_id).await;
        assert!(
            config.read().nats_served_tool_names.read().contains("read"),
            "the refresh must record the raw alias in the served-name cache"
        );
        let functions = config.read().select_tools(&agent_config);
        assert!(
            functions
                .as_deref()
                .unwrap_or_default()
                .iter()
                .all(|declaration| declaration.name != "read"),
            "a cached provider must hide the raw-alias script tool"
        );

        // Cached absence: the refresh clears the cache and the script tool
        // is selectable again.
        cache_discovery(cache, key, None, Instant::now(), registry_epoch());
        refresh_nats_tool_declarations(&config, &instance_id).await;
        assert!(
            config.read().nats_served_tool_names.read().is_empty(),
            "a cached absence must clear the served-name cache"
        );
        let functions = config.read().select_tools(&agent_config);
        assert!(
            functions
                .as_deref()
                .unwrap_or_default()
                .iter()
                .any(|declaration| declaration.name == "read"),
            "a cleared cache must restore the script tool"
        );
        Ok(())
    }

    #[test]
    fn hook_discovery_cache_reuses_arc_until_ttl_expires() {
        let cache = std::sync::Mutex::new(HashMap::new());
        let instance_id = ServerScope::from_string("cache-test");
        let discovered_at = Instant::now();
        let first = Arc::new(());
        cache_discovery(
            &cache,
            instance_id.clone(),
            Some(Arc::clone(&first)),
            discovered_at,
            0,
        );

        let cached = cached_discovery(
            &cache,
            &instance_id,
            discovered_at + Duration::from_secs(29),
            Duration::from_secs(30),
            0,
        )
        .flatten()
        .expect("fresh provider cached");
        assert!(Arc::ptr_eq(&first, &cached));

        assert!(cached_discovery(
            &cache,
            &instance_id,
            discovered_at + Duration::from_secs(30),
            Duration::from_secs(30),
            0
        )
        .is_none());
        let refreshed = Arc::new(());
        cache_discovery(
            &cache,
            instance_id.clone(),
            Some(Arc::clone(&refreshed)),
            discovered_at + Duration::from_secs(30),
            0,
        );
        let cached = cached_discovery(
            &cache,
            &instance_id,
            discovered_at + Duration::from_secs(31),
            Duration::from_secs(30),
            0,
        )
        .flatten()
        .expect("refreshed provider cached");
        assert!(Arc::ptr_eq(&refreshed, &cached));
        assert!(!Arc::ptr_eq(&first, &cached));
    }

    #[test]
    fn tool_discovery_cache_populates_and_expires_at_refresh_interval() {
        let cache = std::sync::Mutex::new(HashMap::new());
        let instance_id = ServerScope::from_string("tool-cache-test");
        let discovered_at = Instant::now();
        let provider = Arc::new(());
        cache_discovery(
            &cache,
            instance_id.clone(),
            Some(Arc::clone(&provider)),
            discovered_at,
            0,
        );

        let cached = cached_discovery(
            &cache,
            &instance_id,
            discovered_at + REGISTRATION_REFRESH_INTERVAL - Duration::from_millis(1),
            REGISTRATION_REFRESH_INTERVAL,
            0,
        )
        .flatten()
        .expect("fresh tool provider cached");
        assert!(Arc::ptr_eq(&provider, &cached));
        assert!(cached_discovery(
            &cache,
            &instance_id,
            discovered_at + REGISTRATION_REFRESH_INTERVAL,
            REGISTRATION_REFRESH_INTERVAL,
            0
        )
        .is_none());
    }

    #[test]
    fn discovery_cache_prunes_expired_entries_during_insert() {
        let cache = std::sync::Mutex::new(HashMap::<String, CachedDiscovery<()>>::new());
        let discovered_at = Instant::now();
        cache_discovery(&cache, "expired".into(), None, discovered_at, 0);
        cache_discovery(
            &cache,
            "fresh".into(),
            None,
            discovered_at + REGISTRATION_REFRESH_INTERVAL - Duration::from_millis(1),
            0,
        );

        cache_discovery(
            &cache,
            "new".into(),
            None,
            discovered_at + REGISTRATION_REFRESH_INTERVAL,
            0,
        );

        let cache = cache.lock().expect("cache lock");
        assert_eq!(cache.len(), 2);
        assert!(!cache.contains_key("expired"));
        assert!(cache.contains_key("fresh"));
        assert!(cache.contains_key("new"));
    }

    #[test]
    fn discovery_cache_evicts_oldest_entry_at_capacity() {
        let cache = std::sync::Mutex::new(HashMap::new());
        let discovered_at = Instant::now();
        let refreshed_provider = Arc::new("refreshed");
        for key in 0..DISCOVERY_CACHE_MAX_ENTRIES {
            cache_discovery(
                &cache,
                key,
                None::<Arc<&str>>,
                discovered_at + Duration::from_millis(key as u64),
                0,
            );
        }
        cache_discovery(
            &cache,
            0,
            Some(Arc::clone(&refreshed_provider)),
            discovered_at + Duration::from_millis(DISCOVERY_CACHE_MAX_ENTRIES as u64),
            0,
        );

        cache_discovery(
            &cache,
            DISCOVERY_CACHE_MAX_ENTRIES,
            None,
            discovered_at + Duration::from_millis(DISCOVERY_CACHE_MAX_ENTRIES as u64 + 1),
            0,
        );

        let cache = cache.lock().expect("cache lock");
        assert_eq!(cache.len(), DISCOVERY_CACHE_MAX_ENTRIES);
        assert!(cache.contains_key(&0));
        assert!(!cache.contains_key(&1));
        assert!(cache.contains_key(&DISCOVERY_CACHE_MAX_ENTRIES));
        assert!(Arc::ptr_eq(
            &refreshed_provider,
            cache
                .get(&0)
                .and_then(|entry| entry.provider.as_ref())
                .expect("refreshed entry retained")
        ));
    }

    #[test]
    fn discovery_cache_invalidates_when_registry_epoch_moves() {
        let cache = std::sync::Mutex::new(HashMap::new());
        let instance_id = ServerScope::from_string("epoch-invalidation-test");
        let discovered_at = Instant::now();
        let provider = Arc::new(());
        cache_discovery(
            &cache,
            instance_id.clone(),
            Some(Arc::clone(&provider)),
            discovered_at,
            7,
        );

        // Fresh within the TTL, but the registry watch bumped the epoch: the
        // entry is stale (decision 5b — next-completion freshness).
        assert!(cached_discovery(
            &cache,
            &instance_id,
            discovered_at + Duration::from_secs(1),
            REGISTRATION_REFRESH_INTERVAL,
            8,
        )
        .is_none());

        // Same epoch: the entry is still reused.
        let cached = cached_discovery(
            &cache,
            &instance_id,
            discovered_at + Duration::from_secs(2),
            REGISTRATION_REFRESH_INTERVAL,
            7,
        )
        .flatten()
        .expect("same-epoch entry reused");
        assert!(Arc::ptr_eq(&provider, &cached));
    }

    #[test]
    fn discovery_cache_invalidates_when_registry_revision_moves() {
        let cache = std::sync::Mutex::new(HashMap::new());
        let instance_id = ServerScope::from_string("revision-invalidation-test");
        let discovered_at = Instant::now();
        let provider = Arc::new(());
        cache_discovery_with_revision(
            &cache,
            instance_id.clone(),
            CachedDiscovery {
                provider: Some(Arc::clone(&provider)),
                discovered_at,
                epoch: 7,
                revision: Some(12),
            },
        );

        // Fresh within the TTL and the epoch matches, but the registry
        // stream's sequence moved (a write or delete acked after the scan
        // started): the deterministic decision-5b check rejects the entry
        // even though the async watch hasn't delivered anything (the epoch
        // still matches).
        assert!(cached_discovery_matching(
            &cache,
            &instance_id,
            discovered_at + Duration::from_secs(1),
            REGISTRATION_REFRESH_INTERVAL,
            |entry| entry.epoch == 7 && entry.revision == Some(13),
        )
        .is_none());

        // Same epoch and sequence: the entry is still reused.
        let cached = cached_discovery_matching(
            &cache,
            &instance_id,
            discovered_at + Duration::from_secs(2),
            REGISTRATION_REFRESH_INTERVAL,
            |entry| entry.epoch == 7 && entry.revision == Some(12),
        )
        .flatten()
        .expect("unchanged registry entry reused");
        assert!(Arc::ptr_eq(&provider, &cached));
    }

    #[test]
    fn discovery_cache_rejects_out_of_order_refresh() {
        let cache = std::sync::Mutex::new(HashMap::new());
        let discovered_at = Instant::now();
        let newer_provider = Arc::new("newer");
        cache_discovery(
            &cache,
            "key",
            Some(Arc::clone(&newer_provider)),
            discovered_at + Duration::from_secs(1),
            0,
        );

        cache_discovery(&cache, "key", Some(Arc::new("older")), discovered_at, 0);

        let cache = cache.lock().expect("cache lock");
        let entry = cache.get("key").expect("newer entry retained");
        assert_eq!(entry.discovered_at, discovered_at + Duration::from_secs(1));
        assert!(Arc::ptr_eq(
            &newer_provider,
            entry.provider.as_ref().expect("newer provider retained")
        ));
    }

    #[test]
    fn failed_discovery_retry_window_opens_closes_and_reopens() {
        harnx_core::require_nextest();
        let config = Config::default();
        let key = tool_discovery_key(&config, &ServerScope::from_string("window-test"), None);
        let failed_at = Instant::now();
        note_discovery_recovered(&key);

        assert!(
            !failed_discovery_window_holds(&key, failed_at),
            "no failure recorded: attempts proceed"
        );

        record_failed_discovery(key.clone(), failed_at);
        assert!(
            failed_discovery_window_holds(
                &key,
                failed_at + FAILED_DISCOVERY_RETRY_INTERVAL - Duration::from_millis(1)
            ),
            "within the interval: the next attempt is suppressed"
        );
        assert!(
            !failed_discovery_window_holds(&key, failed_at + FAILED_DISCOVERY_RETRY_INTERVAL),
            "once the interval elapses, attempts proceed again"
        );

        // A success clears the gate immediately: recovery never waits out
        // the rest of the interval, unlike a negative cache entry.
        note_discovery_recovered(&key);
        assert!(
            !failed_discovery_window_holds(&key, failed_at + Duration::from_millis(1)),
            "a successful discovery reopens the window at once"
        );
    }

    /// The failure window is keyed by discovery scope: one worker serves
    /// many sessions on one broker, and a failed discovery under one scope
    /// must not leave another's completions without tools (G1).
    #[test]
    fn failed_discovery_window_is_keyed_by_scope() {
        harnx_core::require_nextest();
        let config = Config::default();
        let key_a = tool_discovery_key(&config, &ServerScope::from_string("scope-a"), None);
        let key_b = tool_discovery_key(&config, &ServerScope::from_string("scope-b"), None);
        let failed_at = Instant::now();
        note_discovery_recovered(&key_a);
        note_discovery_recovered(&key_b);

        record_failed_discovery(key_a.clone(), failed_at);
        assert!(
            failed_discovery_window_holds(&key_a, failed_at + Duration::from_millis(1)),
            "the failing scope is suppressed for the interval"
        );
        assert!(
            !failed_discovery_window_holds(&key_b, failed_at + Duration::from_millis(1)),
            "a failure under one scope must not suppress another scope's discovery"
        );

        // Recovery is keyed too: scope B's success must not reopen scope A's
        // window.
        note_discovery_recovered(&key_b);
        assert!(failed_discovery_window_holds(
            &key_a,
            failed_at + Duration::from_millis(2)
        ));
        note_discovery_recovered(&key_a);
    }

    #[test]
    fn cached_discovery_snapshot_expires_at_ttl_and_carries_evidence() {
        let cache = std::sync::Mutex::new(HashMap::new());
        let instance_id = ServerScope::from_string("snapshot-evidence-test");
        let discovered_at = Instant::now();
        let provider = Arc::new("provider");
        cache_discovery_with_revision(
            &cache,
            instance_id.clone(),
            CachedDiscovery {
                provider: Some(Arc::clone(&provider)),
                discovered_at,
                epoch: 7,
                revision: Some(12),
            },
        );
        // An entry whose pre-scan revision read failed carries no proof.
        cache_discovery_with_revision(
            &cache,
            ServerScope::from_string("snapshot-unproven-test"),
            CachedDiscovery {
                provider: Some(Arc::new("unproven")),
                discovered_at,
                epoch: 7,
                revision: None,
            },
        );

        let snapshot = cached_discovery_snapshot(
            &cache,
            &instance_id,
            discovered_at + Duration::from_secs(1),
            REGISTRATION_REFRESH_INTERVAL,
        )
        .expect("a live entry yields a snapshot for the freshness check");
        assert!(Arc::ptr_eq(&provider, &snapshot.provider));
        assert_eq!(
            (snapshot.epoch, snapshot.revision),
            (7, Some(12)),
            "the snapshot carries the evidence the probe must re-check"
        );

        let unproven = cached_discovery_snapshot(
            &cache,
            &ServerScope::from_string("snapshot-unproven-test"),
            discovered_at + Duration::from_secs(1),
            REGISTRATION_REFRESH_INTERVAL,
        )
        .expect("an unread revision still yields a snapshot");
        assert_eq!(unproven.revision, None, "without fabricating a 0 sequence");

        assert!(
            cached_discovery_snapshot(
                &cache,
                &instance_id,
                discovered_at + REGISTRATION_REFRESH_INTERVAL,
                REGISTRATION_REFRESH_INTERVAL
            )
            .is_none(),
            "an entry older than the TTL yields no snapshot"
        );
        assert!(
            cached_discovery_snapshot(
                &cache,
                &ServerScope::from_string("never-cached"),
                discovered_at,
                REGISTRATION_REFRESH_INTERVAL
            )
            .is_none(),
            "a missing key yields no snapshot"
        );
    }

    /// The decision-5b matrix for one cached snapshot: which probe results
    /// (revision evidence) keep it current.
    #[test]
    fn snapshot_freshness_decision_matrix() {
        let current = CachedSnapshot {
            provider: Arc::new(()),
            epoch: 7,
            revision: Some(12),
        };

        // Reachable broker, nothing changed: current.
        assert!(snapshot_is_current(&current, 7, Some(12)));

        // The watch epoch moved (some other change under this scope): stale.
        assert!(!snapshot_is_current(&current, 8, Some(12)));

        // The registry stream's sequence moved (a write or delete acked after
        // the snapshot's scan): stale, deterministically.
        assert!(!snapshot_is_current(&current, 7, Some(13)));

        // The whole bucket was deleted: the stream reads as sequence 0, so a
        // snapshot cached with any later revision is stale — the state that
        // must not fall back to the epoch-only path (F1), or removed tools
        // stay advertised until the TTL.
        assert!(!snapshot_is_current(&current, 7, Some(0)));

        // A snapshot of a still-empty registry stays current while it stays
        // empty, and goes stale at the first registration.
        let empty = CachedSnapshot {
            provider: Arc::new(()),
            epoch: 7,
            revision: Some(0),
        };
        assert!(snapshot_is_current(&empty, 7, Some(0)));
        assert!(!snapshot_is_current(&empty, 7, Some(1)));

        // The probe could not reach the broker: only the watch epoch can
        // vouch for the snapshot — the pre-change best-effort behavior.
        assert!(snapshot_is_current(&current, 7, None));
        assert!(!snapshot_is_current(&current, 8, None));

        // The snapshot was cached without revision proof (its pre-scan read
        // failed): any reachable probe invalidates it, forcing one rescan
        // that captures a real sequence — a bucket deletion included, which
        // is why no fabricated 0 may be cached in the proof's place (G3).
        let unproven = CachedSnapshot {
            provider: Arc::new(()),
            epoch: 7,
            revision: None,
        };
        assert!(!snapshot_is_current(&unproven, 7, Some(0)));
        assert!(!snapshot_is_current(&unproven, 7, Some(12)));
        assert!(snapshot_is_current(&unproven, 7, None));
        assert!(!snapshot_is_current(&unproven, 8, None));
    }

    /// A private broker pointed at by `Config::default()`, for the NATS-backed
    /// tests below. Holds the env lock and both env guards for its whole
    /// lifetime (`crate::test_environment`'s contract) and the JetStream
    /// store dir (dropping it under a running server deletes the store from
    /// under it). `None` when the nats-server binary is missing.
    struct BrokerFixture {
        server: crate::nats_worker::tests::TestNatsServer,
        _store: tempfile::TempDir,
        _url_env: crate::test_environment::EnvGuard,
        _token_env: crate::test_environment::EnvGuard,
        // Declared last so it drops after the env guards restore the
        // environment, keeping the lock held for their whole lifetime.
        _lock: tokio::sync::MutexGuard<'static, ()>,
    }

    async fn broker_fixture() -> Option<BrokerFixture> {
        let lock = crate::test_environment::env_lock_async().await;
        let (url, server, store) = crate::nats_worker::tests::spawn_test_nats().await?;
        // The resolver requires the full URL+token handoff pair; the spawned
        // server has no auth configured, so the token value is ignored by it.
        let url_env = crate::test_environment::EnvGuard::new(HARNX_NATS_URL_ENV, &url);
        let token_env =
            crate::test_environment::EnvGuard::new(crate::config::HARNX_NATS_TOKEN_ENV, "unused");
        Some(BrokerFixture {
            server,
            _store: store,
            _lock: lock,
            _url_env: url_env,
            _token_env: token_env,
        })
    }

    /// An unreachable probe must not evict a live snapshot: with no evidence
    /// either way, the watch epoch alone vouches for it (pre-change behavior).
    #[tokio::test]
    async fn nats_cached_lookup_falls_back_to_epoch_when_probe_unreachable() {
        harnx_core::require_nextest();
        let Some(mut fixture) = broker_fixture().await else {
            return;
        };
        let config = Config::default();
        let scope = ServerScope::from_string("epoch-fallback-test");
        let key = tool_discovery_key(&config, &scope, None);
        note_discovery_recovered(&key);

        let first = discover_nats_tool_provider_cached(&config, &scope, None)
            .await
            .expect("fresh discovery against a live broker");
        let second = discover_nats_tool_provider_cached(&config, &scope, None)
            .await
            .expect("second lookup reuses the cache");
        assert!(
            Arc::ptr_eq(&first, &second),
            "an unchanged registry reuses the cached provider"
        );

        // Kill the broker: the next probe cannot reach it, the epoch hasn't
        // moved (nothing changed), and the cached provider is still served
        // rather than dropped. The outer timeout pins the bound: the probe's
        // 2 s context timeout must make this return well before the client's
        // default 10 s request timeout would.
        fixture.server.kill().expect("kill test broker");
        let _ = fixture.server.wait();
        let third = tokio::time::timeout(
            Duration::from_secs(5),
            discover_nats_tool_provider_cached(&config, &scope, None),
        )
        .await
        .expect("the 2 s probe timeout bounds the fallback lookup")
        .expect("probe failure falls back to the epoch, not to discovery");
        assert!(
            Arc::ptr_eq(&first, &third),
            "an unreachable probe must not evict a live snapshot"
        );
        note_discovery_recovered(&key);
    }

    /// A failed fresh discovery neither caches `None` nor drops the cached
    /// snapshot: it just shuts this scope's retry window (F2 policy).
    #[tokio::test]
    async fn nats_failed_fresh_discovery_does_not_poison_the_cache() {
        harnx_core::require_nextest();
        let Some(mut fixture) = broker_fixture().await else {
            return;
        };
        let config = Config::default();
        let scope = ServerScope::from_string("fresh-failure-test");
        let key = tool_discovery_key(&config, &scope, None);
        note_discovery_recovered(&key);
        discover_nats_tool_provider_cached(&config, &scope, None)
            .await
            .expect("discovery against a live broker primes the cache");
        let cache = NATS_TOOL_DISCOVERY_CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
        assert!(
            cached_discovery_snapshot(cache, &key, Instant::now(), REGISTRATION_REFRESH_INTERVAL)
                .is_some(),
            "the primed entry is live before the outage"
        );

        fixture.server.kill().expect("kill test broker");
        let _ = fixture.server.wait();
        let failed = discover_nats_tool_provider_fresh(&config, &scope, None).await;
        assert!(failed.is_none(), "discovery against a dead broker fails");

        // Nothing was written over the primed entry — the next lookup still
        // has fresh evidence to check, instead of a negative entry.
        assert!(
            cached_discovery_snapshot(cache, &key, Instant::now(), REGISTRATION_REFRESH_INTERVAL)
                .is_some(),
            "a failed discovery must not overwrite the cached snapshot"
        );

        // The retry window is shut for the interval: a second attempt is
        // suppressed outright, returning fast without paying another
        // connect — not silently retried (removing the gate would pay the
        // ~15 s recovery timeout and blow this bound).
        let suppressed = tokio::time::timeout(
            Duration::from_secs(1),
            discover_nats_tool_provider_fresh(&config, &scope, None),
        )
        .await
        .expect("the suppressed attempt returns without touching the broker");
        assert!(suppressed.is_none());
        assert!(cached_discovery_snapshot(
            cache,
            &key,
            Instant::now(),
            REGISTRATION_REFRESH_INTERVAL
        )
        .is_some());
        note_discovery_recovered(&key);
    }

    /// The forced activation refresh bypasses even its own scope's closed
    /// failure window (H1): a recent failure here must not delay seeing
    /// registrations the activation just waited for.
    #[tokio::test]
    async fn nats_forced_fresh_discovery_bypasses_a_closed_failure_window() {
        harnx_core::require_nextest();
        let Some(fixture) = broker_fixture().await else {
            return;
        };
        let config = Config::default();
        let scope = ServerScope::from_string("forced-bypass-test");
        let key = tool_discovery_key(&config, &scope, None);
        note_discovery_recovered(&key);
        // Close this scope's window without paying a real outage.
        record_failed_discovery(key.clone(), Instant::now());

        let gated = discover_nats_tool_provider_fresh(&config, &scope, None).await;
        assert!(
            gated.is_none(),
            "the gated refresh stays suppressed for the interval"
        );

        let forced = discover_nats_tool_provider_fresh_forced(&config, &scope, None).await;
        assert!(
            forced.is_some(),
            "the forced refresh bypasses its own closed window"
        );
        assert!(
            !failed_discovery_window_holds(&key, Instant::now()),
            "a forced success clears this scope's failure marker"
        );
        // The forced refresh must cache what it discovered, so the next
        // cached lookup reuses the same snapshot.
        let reused = discover_nats_tool_provider_cached(&config, &scope, None)
            .await
            .expect("the cached lookup reuses the forced snapshot");
        assert!(
            forced.is_some_and(|provider| Arc::ptr_eq(&provider, &reused)),
            "the forced refresh populates the cache"
        );
        let _ = fixture;
    }

    /// A snapshot cached without revision proof (its pre-scan read failed)
    /// is invalidated by any reachable probe: the next lookup rescans and
    /// captures a real sequence, so a later bucket deletion cannot match a
    /// fabricated 0 (G3, integration half of the matrix cell).
    #[tokio::test]
    async fn nats_unread_cached_snapshot_rescans_on_reachable_probe() {
        harnx_core::require_nextest();
        let Some(fixture) = broker_fixture().await else {
            return;
        };
        let config = Config::default();
        let scope = ServerScope::from_string("unread-rescan-test");
        let key = tool_discovery_key(&config, &scope, None);
        note_discovery_recovered(&key);
        let primed = discover_nats_tool_provider_cached(&config, &scope, None)
            .await
            .expect("a live broker primes the cache");

        // Overwrite the entry with what an unread scan would have cached:
        // the same provider, but no revision proof.
        let cache = NATS_TOOL_DISCOVERY_CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
        cache_discovery_with_revision(
            cache,
            key,
            CachedDiscovery {
                provider: Some(Arc::clone(&primed)),
                discovered_at: Instant::now(),
                epoch: registry_epoch(),
                revision: None,
            },
        );

        // The broker is reachable and its registry stream is absent
        // (sequence 0): the unproven snapshot must not survive the probe.
        let rescanned = discover_nats_tool_provider_cached(&config, &scope, None).await;
        assert!(
            rescanned.is_some(),
            "the rescan against a live broker succeeds"
        );
        assert!(
            rescanned.is_some_and(|provider| !Arc::ptr_eq(&primed, &provider)),
            "an unproven snapshot must be invalidated by a reachable probe"
        );
        let _ = fixture;
    }
}
