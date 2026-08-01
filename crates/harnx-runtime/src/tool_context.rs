use crate::agent_loop::AgentLoopContext;
use crate::config::{Config, GlobalConfig, Input};
use crate::nats_hook_provider::NatsHookProvider;
use crate::nats_tool_provider::{NatsInFlightCalls, NatsToolProvider};
use crate::tool::CompletionText;
use crate::utils::AbortSignal;
use harnx_core::instance::InstanceId;
use harnx_hooks::PersistentHookManager;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Shared runtime state for one tool-evaluation round.
pub struct ToolRoundParams<'a> {
    pub config: &'a GlobalConfig,
    pub instance_id: &'a InstanceId,
    pub input: &'a Input,
    pub completion: CompletionText<'a>,
    pub abort_signal: &'a AbortSignal,
    pub persistent_manager: &'a Arc<Mutex<PersistentHookManager>>,
    pub working_dir: Option<&'a Path>,
    pub nats_hook_provider: Option<Arc<NatsHookProvider>>,
    pub pending_async_context: Option<Arc<Mutex<Option<String>>>>,
}

/// Inputs used to assemble the provider and rendering context for a tool round.
pub struct BuildToolEvalContextParams<'a> {
    pub config: &'a GlobalConfig,
    pub instance_id: &'a InstanceId,
    pub agent_use_tools: Option<&'a str>,
    pub current_agent_package: Option<String>,
    pub persistent_manager: &'a Arc<Mutex<PersistentHookManager>>,
    pub working_dir: Option<&'a Path>,
    pub nats_hook_provider: Option<Arc<NatsHookProvider>>,
    pub pending_async_context: Option<Arc<Mutex<Option<String>>>>,
}

impl<'a> BuildToolEvalContextParams<'a> {
    pub fn new(
        config: &'a GlobalConfig,
        instance_id: &'a InstanceId,
        persistent_manager: &'a Arc<Mutex<PersistentHookManager>>,
    ) -> Self {
        Self {
            config,
            instance_id,
            agent_use_tools: None,
            current_agent_package: None,
            persistent_manager,
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
            persistent_manager: &self.persistent_manager,
            working_dir: self.working_dir.as_deref(),
            nats_hook_provider: self.nats_hook_provider.clone(),
            pending_async_context: self.pending_async_context.clone(),
        }
    }
}

const NATS_HOOK_DISCOVERY_TTL: Duration = Duration::from_secs(30);

type HookDiscoveryCache = std::sync::Mutex<HashMap<InstanceId, CachedDiscovery<NatsHookProvider>>>;
static NATS_HOOK_DISCOVERY_CACHE: OnceLock<HookDiscoveryCache> = OnceLock::new();

struct CachedDiscovery<T> {
    provider: Option<Arc<T>>,
    discovered_at: Instant,
}

fn cached_discovery<T>(
    cache: &std::sync::Mutex<HashMap<InstanceId, CachedDiscovery<T>>>,
    instance_id: &InstanceId,
    now: Instant,
    ttl: Duration,
) -> Option<Option<Arc<T>>> {
    let mut cache = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    cache.retain(|_, entry| now.saturating_duration_since(entry.discovered_at) < ttl);
    cache.get(instance_id).map(|entry| entry.provider.clone())
}

fn cache_discovery<T>(
    cache: &std::sync::Mutex<HashMap<InstanceId, CachedDiscovery<T>>>,
    instance_id: InstanceId,
    provider: Option<Arc<T>>,
    discovered_at: Instant,
) {
    cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(
            instance_id,
            CachedDiscovery {
                provider,
                discovered_at,
            },
        );
}

/// Discover NATS hooks at most once per instance during each refresh interval.
pub async fn discover_nats_hook_provider_cached(
    config: &Config,
    instance_id: &InstanceId,
) -> Option<Arc<NatsHookProvider>> {
    let cache = NATS_HOOK_DISCOVERY_CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let now = Instant::now();
    if let Some(provider) = cached_discovery(cache, instance_id, now, NATS_HOOK_DISCOVERY_TTL) {
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

pub async fn discover_nats_tool_provider(
    config: &Config,
    instance_id: &InstanceId,
) -> Option<Arc<NatsToolProvider>> {
    NatsToolProvider::discover(
        config,
        instance_id.clone(),
        NatsInFlightCalls::for_instance(instance_id),
    )
    .await
    .ok()
    .map(Arc::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_discovery_cache_reuses_arc_until_ttl_expires() {
        let cache = std::sync::Mutex::new(HashMap::new());
        let instance_id = InstanceId::from_string("cache-test");
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
    fn hook_discovery_cache_preserves_none_until_ttl_expires() {
        let cache = std::sync::Mutex::new(HashMap::<InstanceId, CachedDiscovery<()>>::new());
        let instance_id = InstanceId::from_string("none-cache-test");
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
