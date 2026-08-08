use crate::agent_loop::AgentLoopContext;
use crate::config::HARNX_NATS_URL_ENV;
use crate::config::{Config, GlobalConfig, Input};
use crate::nats_hook_provider::NatsHookProvider;
use crate::nats_tool_provider::{NatsInFlightCalls, NatsToolProvider};
use crate::tool::CompletionText;
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
    pub nats_hook_provider: Option<Arc<NatsHookProvider>>,
    pub pending_async_context: Option<Arc<Mutex<Option<String>>>>,
}

/// Inputs used to assemble the provider and rendering context for a tool round.
pub struct BuildToolEvalContextParams<'a> {
    pub config: &'a GlobalConfig,
    pub instance_id: &'a ServerScope,
    pub agent_use_tools: Option<&'a str>,
    pub current_agent_package: Option<String>,
    pub working_dir: Option<&'a Path>,
    pub nats_hook_provider: Option<Arc<NatsHookProvider>>,
    pub pending_async_context: Option<Arc<Mutex<Option<String>>>>,
}

impl<'a> BuildToolEvalContextParams<'a> {
    pub fn new(config: &'a GlobalConfig, instance_id: &'a ServerScope) -> Self {
        Self {
            config,
            instance_id,
            agent_use_tools: None,
            current_agent_package: None,
            working_dir: None,
            nats_hook_provider: None,
            pending_async_context: None,
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
            nats_hook_provider: self.nats_hook_provider.clone(),
            pending_async_context: self.pending_async_context.clone(),
        }
    }
}

const REGISTRATION_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

type HookDiscoveryCache = std::sync::Mutex<HashMap<ServerScope, CachedDiscovery<NatsHookProvider>>>;
type ToolDiscoveryKey = (ServerScope, Option<String>);
type ToolDiscoveryCache =
    std::sync::Mutex<HashMap<ToolDiscoveryKey, CachedDiscovery<NatsToolProvider>>>;
static NATS_HOOK_DISCOVERY_CACHE: OnceLock<HookDiscoveryCache> = OnceLock::new();
static NATS_TOOL_DISCOVERY_CACHE: OnceLock<ToolDiscoveryCache> = OnceLock::new();

struct CachedDiscovery<T> {
    provider: Option<Arc<T>>,
    discovered_at: Instant,
}

fn cached_discovery<K: Eq + std::hash::Hash, T>(
    cache: &std::sync::Mutex<HashMap<K, CachedDiscovery<T>>>,
    key: &K,
    now: Instant,
    ttl: Duration,
) -> Option<Option<Arc<T>>> {
    let mut cache = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    cache.retain(|_, entry| now.saturating_duration_since(entry.discovered_at) < ttl);
    cache.get(key).map(|entry| entry.provider.clone())
}

fn cache_discovery<K: Eq + std::hash::Hash, T>(
    cache: &std::sync::Mutex<HashMap<K, CachedDiscovery<T>>>,
    key: K,
    provider: Option<Arc<T>>,
    discovered_at: Instant,
) {
    cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(
            key,
            CachedDiscovery {
                provider,
                discovered_at,
            },
        );
}

/// Discover NATS hooks at most once per instance during each refresh interval.
pub async fn discover_nats_hook_provider_cached(
    config: &Config,
    instance_id: &ServerScope,
) -> Option<Arc<NatsHookProvider>> {
    let cache = NATS_HOOK_DISCOVERY_CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let now = Instant::now();
    if let Some(provider) = cached_discovery(cache, instance_id, now, REGISTRATION_REFRESH_INTERVAL)
    {
        return provider;
    }

    // Don't hold the process-wide lock while connecting to NATS or scanning KV.
    let provider = NatsHookProvider::discover(config, instance_id.clone())
        .await
        .ok()
        .map(Arc::new);
    cache_discovery(cache, instance_id.clone(), provider.clone(), Instant::now());
    provider
}

pub async fn discover_nats_tool_provider_cached(
    config: &Config,
    instance_id: &ServerScope,
    active_package: Option<&str>,
) -> Option<Arc<NatsToolProvider>> {
    let cache = NATS_TOOL_DISCOVERY_CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let key = (instance_id.clone(), active_package.map(str::to_string));
    let now = Instant::now();
    if let Some(provider) = cached_discovery(cache, &key, now, REGISTRATION_REFRESH_INTERVAL) {
        return provider;
    }

    // Don't hold the process-wide lock while connecting to NATS or scanning KV.
    let provider = NatsToolProvider::discover(
        config,
        instance_id.clone(),
        NatsInFlightCalls::for_instance(instance_id),
        active_package,
    )
    .await
    .ok()
    .map(Arc::new);
    cache_discovery(cache, key, provider.clone(), Instant::now());
    provider
}

/// Refresh declarations before completion request construction.
pub async fn refresh_nats_tool_declarations(config: &GlobalConfig, instance_id: &ServerScope) {
    // Gate on the broker address, not on `HARNX_SERVER_SCOPE`. The worker creates
    // its instance id in-process and only ever exports it to the children it
    // spawns, so an instance-id check is false in the one process that runs this
    // — every turn discovered zero tools and the model saw built-ins only.
    //
    // A guard is still needed: with no broker address `resolve_local_nats_server_config`
    // falls back to starting a shared NATS server, which a plain front-end must
    // never do just to build a tool list. `HARNX_NATS_URL` is set on the worker
    // by its supervisor and inherited by its children, so it marks exactly the
    // processes that already have a broker to talk to.
    if std::env::var_os(HARNX_NATS_URL_ENV).is_none() {
        return;
    }

    let config_snapshot = config.read().clone();
    let active_package = config_snapshot.active_package();
    let provider = discover_nats_tool_provider_cached(
        &config_snapshot,
        instance_id,
        active_package.as_deref(),
    )
    .await;
    if let Some(provider) = &provider {
        // A mismatched scope is silent otherwise: the worker scans one prefix,
        // a typo'd server registers under another, and this turn's declarations
        // just come back empty — indistinguishable from having no tools
        // configured at all.
        let report = provider.discovery_report();
        if report.found == 0 {
            log::warn!("{}", report.message);
        } else {
            log::debug!("{}", report.message);
        }
    }
    let declarations = provider
        .as_ref()
        .map(|provider| provider.declarations_for_use_tools(Some("*")))
        .unwrap_or_default();
    *config.read().nats_tool_declarations.write() = declarations;
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
        );

        let cached = cached_discovery(
            &cache,
            &instance_id,
            discovered_at + Duration::from_secs(29),
            Duration::from_secs(30),
        )
        .flatten()
        .expect("fresh provider cached");
        assert!(Arc::ptr_eq(&first, &cached));

        assert!(cached_discovery(
            &cache,
            &instance_id,
            discovered_at + Duration::from_secs(30),
            Duration::from_secs(30),
        )
        .is_none());
        let refreshed = Arc::new(());
        cache_discovery(
            &cache,
            instance_id.clone(),
            Some(Arc::clone(&refreshed)),
            discovered_at + Duration::from_secs(30),
        );
        let cached = cached_discovery(
            &cache,
            &instance_id,
            discovered_at + Duration::from_secs(31),
            Duration::from_secs(30),
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
        );

        let cached = cached_discovery(
            &cache,
            &instance_id,
            discovered_at + REGISTRATION_REFRESH_INTERVAL - Duration::from_millis(1),
            REGISTRATION_REFRESH_INTERVAL,
        )
        .flatten()
        .expect("fresh tool provider cached");
        assert!(Arc::ptr_eq(&provider, &cached));
        assert!(cached_discovery(
            &cache,
            &instance_id,
            discovered_at + REGISTRATION_REFRESH_INTERVAL,
            REGISTRATION_REFRESH_INTERVAL,
        )
        .is_none());
    }

    #[test]
    fn hook_discovery_cache_preserves_none_until_ttl_expires() {
        let cache = std::sync::Mutex::new(HashMap::<ServerScope, CachedDiscovery<()>>::new());
        let instance_id = ServerScope::from_string("none-cache-test");
        let discovered_at = Instant::now();
        cache_discovery(&cache, instance_id.clone(), None, discovered_at);

        assert!(matches!(
            cached_discovery(
                &cache,
                &instance_id,
                discovered_at + Duration::from_secs(1),
                Duration::from_secs(30),
            ),
            Some(None)
        ));
    }
}
