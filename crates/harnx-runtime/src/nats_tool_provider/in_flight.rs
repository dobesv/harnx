//! Per-server call liveness, independent of turn and cleanup lifetimes.
use harnx_core::instance::ServerScope;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, Weak};
use tokio::sync::{oneshot, Mutex};
#[derive(Clone, Debug)]
pub(super) enum InFlightFailure {
    Unavailable(String),
}

type InFlightMap = Mutex<HashMap<String, InFlightCall>>;
static INSTANCE_IN_FLIGHT: OnceLock<std::sync::Mutex<HashMap<ServerScope, Weak<InFlightMap>>>> =
    OnceLock::new();

/// Shared handle used by tool-process supervision to fail active NATS calls.
#[derive(Clone, Default)]
pub struct NatsInFlightCalls {
    calls: Arc<InFlightMap>,
}

struct InFlightCall {
    server: String,
    failure: oneshot::Sender<InFlightFailure>,
}

impl NatsInFlightCalls {
    /// Return the process-wide handle shared by provider and supervisor for an instance.
    pub fn for_instance(instance_id: &ServerScope) -> Self {
        let registry = INSTANCE_IN_FLIGHT.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
        let mut registry = registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        registry.retain(|_, calls| calls.strong_count() > 0);
        if let Some(calls) = registry.get(instance_id).and_then(Weak::upgrade) {
            return Self { calls };
        }
        let calls = Arc::new(Mutex::new(HashMap::new()));
        registry.insert(instance_id.clone(), Arc::downgrade(&calls));
        Self { calls }
    }

    pub(super) async fn register(
        &self,
        call_id: String,
        server: String,
    ) -> oneshot::Receiver<InFlightFailure> {
        let (failure, receiver) = oneshot::channel();
        self.calls
            .lock()
            .await
            .insert(call_id, InFlightCall { server, failure });
        receiver
    }

    pub(super) async fn complete(&self, call_id: &str) {
        self.calls.lock().await.remove(call_id);
    }

    /// Fail current calls routed to a supervised server that became unavailable.
    pub async fn fail_server_unavailable(&self, server: &str, message: impl Into<String>) {
        let message = message.into();
        let failures = {
            let mut calls = self.calls.lock().await;
            let call_ids = calls
                .iter()
                .filter(|(_, call)| call.server == server)
                .map(|(call_id, _)| call_id.clone())
                .collect::<Vec<_>>();
            call_ids
                .into_iter()
                .filter_map(|call_id| calls.remove(&call_id).map(|call| call.failure))
                .collect::<Vec<_>>()
        };
        for failure in failures {
            let _ = failure.send(InFlightFailure::Unavailable(message.clone()));
        }
    }
}
