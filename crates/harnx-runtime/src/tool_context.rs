use crate::agent_loop::AgentLoopContext;
use crate::config::HARNX_NATS_URL_ENV;
use crate::config::{Config, GlobalConfig, Input};
use crate::nats_tool_provider::{NatsInFlightCalls, NatsToolProvider};
use crate::tool::{CompletionText, PrepareHandoffFn};
use crate::tool_registry_watch::{registry_epoch, registry_state_revision};
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
    revision: u64,
}

// Only the tests below use the epoch-only forms; production goes through the
// revision-aware matching variants.
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

/// [`cached_discovery`] with a caller-supplied freshness predicate, so the
/// NATS path can require the registry revision to match as well (decision
/// 5b's deterministic half).
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
            revision: 0,
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

/// Freshness predicate for a cached snapshot (decision 5b).
///
/// `revision` is the registry stream's sequence as of *now*: `Some(seq)` pins
/// both the watch epoch and the sequence, and `None` means the freshness check
/// could not reach the broker, so only the watch epoch can vouch for the
/// snapshot — the pre-change best-effort behavior.
fn discovery_snapshot_is_current<T>(
    entry: &CachedDiscovery<T>,
    epoch: u64,
    revision: Option<u64>,
) -> bool {
    match revision {
        Some(revision) => entry.epoch == epoch && entry.revision == revision,
        None => entry.epoch == epoch,
    }
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
    let now = Instant::now();
    let epoch = registry_epoch();
    let revision = registry_freshness_revision(config).await;
    match cached_discovery_matching(cache, &key, now, REGISTRATION_REFRESH_INTERVAL, |entry| {
        discovery_snapshot_is_current(entry, epoch, revision)
    }) {
        Some(provider) => provider,
        None => discover_nats_tool_provider_fresh(config, instance_id, active_package).await,
    }
}

/// The registry stream revision the cached snapshot must match, or `None`
/// when the freshness check itself can't reach the broker; discovery would
/// fail the same way, so serving the cached snapshot (when the watch still
/// agrees) is the pre-change best-effort behavior.
// Type-erased box: the revision probe reaches the async-nats connect chain,
// and a concrete boxed future would still push dependent crates' trait-solver
// recursion budget over the limit.
fn registry_freshness_revision<'a>(
    config: &'a Config,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<u64>> + Send + 'a>> {
    Box::pin(async move {
        match registry_state_revision(config).await {
            Ok(revision) => Some(revision.unwrap_or(0)),
            Err(error) => {
                log::debug!("tool registry freshness check failed: {error:#}");
                None
            }
        }
    })
}

/// Refresh tool discovery immediately and replace the cached snapshot.
///
/// Worker activations call this after reconciling an agent's tool servers;
/// using a snapshot captured while those servers were still starting would
/// leave their declarations invisible for the cache TTL.
pub(crate) async fn discover_nats_tool_provider_fresh(
    config: &Config,
    instance_id: &ServerScope,
    active_package: Option<&str>,
) -> Option<Arc<NatsToolProvider>> {
    let cache = NATS_TOOL_DISCOVERY_CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let key = tool_discovery_key(config, instance_id, active_package);
    // Read the registry sequence *before* the scan (decision 5b). A write the
    // broker acked before this read is in both the scan and the sequence; a
    // write acked after it bumps the sequence past the stored value, so the
    // next completion's comparison catches it and rescans. Reading it after
    // the scan would hide a write that landed mid-scan and was missed by it.
    let revision = registry_state_revision(config)
        .await
        .ok()
        .flatten()
        .unwrap_or(0);
    // Don't hold the process-wide lock while connecting to NATS or scanning KV.
    let provider = match NatsToolProvider::discover(
        config,
        instance_id.clone(),
        NatsInFlightCalls::for_instance(instance_id),
        active_package,
    )
    .await
    {
        Ok(provider) => Some(Arc::new(provider)),
        Err(error) => {
            // A connect/subscribe failure here is distinct from "no tools
            // registered" (handled inside `discover` itself) — without this,
            // it produced no provider and no log, the same silent-degradation
            // this cache exists to avoid.
            log::warn!(
                "NATS tool discovery failed under scope '{}': {error:#}",
                instance_id.as_str()
            );
            None
        }
    };
    cache_discovery_with_revision(
        cache,
        key,
        CachedDiscovery {
            provider: provider.clone(),
            discovered_at: Instant::now(),
            epoch: registry_epoch(),
            revision,
        },
    );
    provider
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

// Type-erased box for the same reason as `registry_freshness_revision`.
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
        let declarations = declarations_for_all_tools(provider.as_deref());
        *config.read().nats_tool_declarations.write() = declarations;
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
) -> Vec<harnx_core::tool::ToolDeclaration> {
    provider
        .map(|provider| provider.declarations_for_use_tools(Some("*")))
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
                revision: 12,
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
            |entry| entry.epoch == 7 && entry.revision == 13,
        )
        .is_none());

        // Same epoch and sequence: the entry is still reused.
        let cached = cached_discovery_matching(
            &cache,
            &instance_id,
            discovered_at + Duration::from_secs(2),
            REGISTRATION_REFRESH_INTERVAL,
            |entry| entry.epoch == 7 && entry.revision == 12,
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
    fn hook_discovery_cache_preserves_none_until_ttl_expires() {
        let cache = std::sync::Mutex::new(HashMap::<ServerScope, CachedDiscovery<()>>::new());
        let instance_id = ServerScope::from_string("none-cache-test");
        let discovered_at = Instant::now();
        cache_discovery(&cache, instance_id.clone(), None, discovered_at, 0);

        assert!(matches!(
            cached_discovery(
                &cache,
                &instance_id,
                discovered_at + Duration::from_secs(1),
                Duration::from_secs(30),
                0
            ),
            Some(None)
        ));
    }
}
