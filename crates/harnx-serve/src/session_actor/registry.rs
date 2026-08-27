//! The session registry: one actor per agent/session key, spawned on demand and reaped when idle.

use super::{spawn_session_actor, SessionActorConfig, SessionHandle, SessionKey, SessionMap};
use dashmap::{mapref::entry::Entry, DashMap};
use harnx_runtime::{config::Config, AgentCallFn};
use std::{sync::Arc, time::Duration};
use tokio::sync::Mutex;

/// How long an actor stays alive with no subscribers and nothing running.
const DEFAULT_REAP_TTL: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct SessionRegistry {
    pub(super) map: SessionMap,
    reap_ttl: Duration,
    actor_config: SessionActorConfig,
}

impl SessionRegistry {
    pub fn new(base_config: Config) -> Self {
        Self::with_reap_ttl(base_config, DEFAULT_REAP_TTL)
    }

    pub fn with_reap_ttl(base_config: Config, reap_ttl: Duration) -> Self {
        Self::with_options(
            reap_ttl,
            SessionActorConfig {
                base_config,
                call_fn: None,
                local_worker: Arc::new(Mutex::new(None)),
            },
        )
    }

    fn with_options(reap_ttl: Duration, actor_config: SessionActorConfig) -> Self {
        Self {
            map: Arc::new(DashMap::new()),
            reap_ttl,
            actor_config,
        }
    }

    pub fn new_for_tests(
        base_config: Config,
        reap_ttl: Duration,
        call_fn: Option<AgentCallFn>,
    ) -> Self {
        Self::with_options(
            reap_ttl,
            SessionActorConfig {
                base_config,
                call_fn,
                local_worker: Arc::new(Mutex::new(None)),
            },
        )
    }

    // Only used by the Unix-only NATS serve tests (`nats_tests`).
    #[cfg(all(test, unix))]
    pub(super) fn new_with_local_worker_for_tests(
        base_config: Config,
        supervisor: harnx_runtime::local_orchestrator::LocalWorkerSupervisor,
    ) -> Self {
        Self::with_options(
            DEFAULT_REAP_TTL,
            SessionActorConfig {
                base_config,
                call_fn: None,
                local_worker: Arc::new(Mutex::new(Some(supervisor))),
            },
        )
    }

    pub fn has_session(&self, key: &SessionKey) -> bool {
        self.map
            .get(key)
            .is_some_and(|handle| !handle.tx.is_closed())
    }

    pub fn get_or_spawn(&self, key: SessionKey) -> SessionHandle {
        get_or_spawn_in(&self.map, key, self.reap_ttl, &self.actor_config)
    }
}

impl Default for SessionRegistry {
    fn default() -> Self {
        Self::new(Config::default())
    }
}

/// Hand out a live handle for `key`, spawning an actor when the key has none.
///
/// An entry whose channel is already closed counts as no actor at all: the actor behind it has
/// stopped, so every command sent through that handle would fail. Replacing it keeps a session
/// usable even if its actor task died without deregistering (a panic, say) instead of failing
/// every later request for that key.
pub(super) fn get_or_spawn_in(
    map: &SessionMap,
    key: SessionKey,
    reap_ttl: Duration,
    actor_config: &SessionActorConfig,
) -> SessionHandle {
    match map.entry(key.clone()) {
        Entry::Occupied(mut entry) if entry.get().tx.is_closed() => {
            log::warn!(
                "session actor for {}/{} stopped without deregistering; spawning a replacement",
                key.agent,
                key.session
            );
            let handle = spawn_session_actor(key, Arc::clone(map), reap_ttl, actor_config.clone());
            entry.insert(handle.clone());
            handle
        }
        Entry::Occupied(entry) => entry.get().clone(),
        Entry::Vacant(entry) => {
            let handle = spawn_session_actor(key, Arc::clone(map), reap_ttl, actor_config.clone());
            entry.insert(handle.clone());
            handle
        }
    }
}
