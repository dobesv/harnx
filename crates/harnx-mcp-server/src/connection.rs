//! Connection-owned reservation, catalog snapshots, and call cleanup.
use crate::{bootstrap::Bootstrap, handler::tool_result};
use harnx_core::{
    abort::{create_abort_signal, AbortSignal},
    tool::ToolDeclaration,
};
use harnx_runtime::{
    config::Config,
    nats_tool_provider::{NatsInFlightCalls, NatsToolProvider},
    nats_worker::tool_reservation::ToolReservationView,
    tool_reservation_client::{ToolReservationHandle, ToolReservationState},
};
use rmcp::model::{CallToolResult, ErrorData, Tool, ToolAnnotations};
use serde_json::Value;
use std::{collections::HashSet, sync::Arc};
use tokio::sync::Mutex;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

struct Catalog {
    state: ToolReservationState,
    provider: Arc<NatsToolProvider>,
    tools: Vec<Tool>,
}

#[derive(Default)]
struct State {
    reservation: Option<ToolReservationHandle>,
    catalog: Option<Arc<Catalog>>,
    closed: bool,
}

impl State {
    fn cached_catalog(&self, current: &ToolReservationState, fresh: bool) -> Option<Arc<Catalog>> {
        self.catalog
            .as_ref()
            .filter(|catalog| !fresh && catalog.state == *current)
            .cloned()
    }
}

/// One backing session per instance. Clones of the handler share this instance;
/// create a new instance for every MCP session, not for every HTTP request.
pub struct Connection {
    bootstrap: Arc<Bootstrap>,
    view: ToolReservationView,
    state: Mutex<State>,
    shutdown: CancellationToken,
    calls: TaskTracker,
}

impl Connection {
    /// Construction is lazy: initialize alone doesn't boot workers or tools.
    pub fn new(bootstrap: Arc<Bootstrap>, view: ToolReservationView) -> Self {
        Self {
            bootstrap,
            view,
            state: Mutex::new(State::default()),
            shutdown: CancellationToken::new(),
            calls: TaskTracker::new(),
        }
    }

    /// Cancel calls immediately, including while another request discovers tools.
    pub fn cancel(&self) {
        self.shutdown.cancel();
    }

    /// Stop admission, drain cancellation, stop renewal, then release reservation.
    /// Idempotent; the durable backing session remains under normal retention.
    pub async fn close(&self) -> anyhow::Result<()> {
        self.cancel();
        let mut state = self.state.lock().await;
        state.closed = true;
        state.catalog = None;
        self.calls.close();
        self.calls.wait().await;
        if let Some(mut reservation) = state.reservation.take() {
            reservation.close().await?;
        }
        Ok(())
    }

    /// Backing session id, if a list/call has opened the lazy reservation.
    pub async fn session_id(&self) -> Option<String> {
        self.state
            .lock()
            .await
            .reservation
            .as_ref()
            .map(|r| r.session_id().to_owned())
    }

    /// Every list request discovers anew, even within the same generation.
    pub async fn list_tools(&self) -> Result<Vec<Tool>, ErrorData> {
        let mut state = self.state.lock().await;
        let catalog = self.catalog(&mut state, true).await?;
        Ok(catalog.tools.clone())
    }

    /// Route exactly once through the advertised snapshot. Cancellation doesn't
    /// drop the provider future: it must run its NATS cancellation cleanup.
    pub async fn call_tool(
        &self,
        name: String,
        arguments: Value,
        cancellation: CancellationToken,
    ) -> Result<CallToolResult, ErrorData> {
        let task = {
            let mut state = self.state.lock().await;
            let catalog = self.catalog(&mut state, false).await?;
            if !catalog.tools.iter().any(|tool| tool.name.as_ref() == name) {
                return Err(ErrorData::invalid_params(
                    format!("unknown tool: {name}"),
                    None,
                ));
            }
            // Recheck after discovery and before admitting a new call. Renewal
            // invalidation must not authorize another call on the old provider.
            if state.reservation.as_ref().map(|r| r.state()) != Some(catalog.state.clone()) {
                state.catalog = None;
                return Err(unavailable(
                    "tool reservation changed during call admission; retry the request",
                ));
            }
            if self.shutdown.is_cancelled() || cancellation.is_cancelled() {
                return Err(unavailable("connection or request cancelled"));
            }
            // Only this external request adapter grants a root. Reservation/catalog lifetime
            // isn't execution lifetime, and delegated work must retain this frozen scope.
            let record = state
                .reservation
                .as_ref()
                .expect("reservation opened")
                .admit_external_call()
                .await
                .map_err(|error| {
                    unavailable(format!("external run admission failed: {error:#}"))
                })?;
            if self.shutdown.is_cancelled()
                || cancellation.is_cancelled()
                || state.reservation.as_ref().map(|r| r.state()) != Some(catalog.state.clone())
            {
                return Err(unavailable(
                    "connection, request, or reservation changed during run admission",
                ));
            }
            let abort = create_abort_signal();
            let guard = AbortOnDrop(abort.clone());
            let shutdown = self.shutdown.clone();
            let call_id = uuid::Uuid::new_v4().to_string();
            let task = self.calls.spawn(async move {
                let call = catalog
                    .provider
                    .call_tool_with_external_admission(&name, arguments, &call_id, &record, &abort);
                tokio::pin!(call);
                let result = tokio::select! {
                    result = &mut call => result,
                    _ = shutdown.cancelled() => { abort.set_ctrlc(); call.await },
                    _ = cancellation.cancelled() => { abort.set_ctrlc(); call.await },
                };
                tool_result(result)
            });
            (task, guard)
        };
        let (task, _guard) = task;
        task.await
            .map_err(|error| unavailable(format!("tool call task failed: {error}")))
    }

    async fn catalog(&self, state: &mut State, fresh: bool) -> Result<Arc<Catalog>, ErrorData> {
        if state.closed || self.shutdown.is_cancelled() {
            return Err(unavailable("connection closed"));
        }
        if state.reservation.is_none() {
            state.reservation = Some(
                self.bootstrap
                    .open_reservation(self.view.clone())
                    .await
                    .map_err(|e| unavailable(format!("tool reservation failed: {e:#}")))?,
            );
        }
        let reservation = state.reservation.as_ref().expect("reservation opened");
        let current = reservation.state();
        let Some(scope) = current.server_scope.clone() else {
            state.catalog = None;
            return Err(unavailable(
                "tool reservation unavailable; waiting for renewal recovery",
            ));
        };
        if let Some(catalog) = state.cached_catalog(&current, fresh) {
            return Ok(catalog);
        }
        // Never leave an earlier advertised snapshot usable after a failed refresh.
        state.catalog = None;
        let provider = NatsToolProvider::discover(
            reservation.config(),
            scope.clone(),
            NatsInFlightCalls::default(),
            self.view.package.as_deref(),
        )
        .await
        .map_err(|e| unavailable(format!("tool discovery failed: {e:#}")))?;
        let tools = selected_tools(reservation.config(), &self.view, provider.declarations())?;
        if reservation.state() != current || self.shutdown.is_cancelled() {
            return Err(unavailable(
                "tool reservation changed during discovery; retry the request",
            ));
        }
        let catalog = Arc::new(Catalog {
            state: current,
            provider: Arc::new(provider),
            tools,
        });
        state.catalog = Some(catalog.clone());
        Ok(catalog)
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // Running tasks own only the snapshot/token, not Connection. Drain them
        // before release when a transport drops its last handler without close.
        self.shutdown.cancel();
        self.calls.close();
        let state = self.state.get_mut();
        state.catalog = None;
        if let Some(mut reservation) = state.reservation.take() {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                let calls = self.calls.clone();
                runtime.spawn(async move {
                    calls.wait().await;
                    if let Err(error) = reservation.close().await {
                        tracing::warn!(%error, "MCP connection cleanup failed");
                    }
                });
            }
            // Without a runtime, handle Drop stops renewal; TTL covers release.
        }
    }
}

struct AbortOnDrop(AbortSignal);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.set_ctrlc();
    }
}

fn unavailable(message: impl Into<String>) -> ErrorData {
    ErrorData::internal_error(message.into(), None)
}

fn selected_tools(
    config: &Config,
    view: &ToolReservationView,
    declarations: &[ToolDeclaration],
) -> Result<Vec<Tool>, ErrorData> {
    let mut config = config.clone();
    // Config::clone shares this cache. Detach before installing package-named
    // declarations so concurrent connections cannot change each other's view.
    config.nats_tool_declarations = Arc::new(parking_lot::RwLock::new(declarations.to_vec()));
    let selected: HashSet<_> = config
        .select_tools_for_package(&view.use_tools, view.package.as_deref())
        .into_iter()
        .map(|d| d.name)
        .collect();
    // Use the provider's schemas, not similarly named local declarations. Raw
    // routing aliases and generated handoff/history declarations aren't exports.
    declarations
        .iter()
        .filter(|d| selected.contains(&d.name))
        .map(|d| {
            let schema =
                serde_json::to_value(&d.parameters).map_err(|e| unavailable(e.to_string()))?;
            let Value::Object(schema) = schema else {
                return Err(unavailable(format!("invalid input schema for {}", d.name)));
            };
            let mut annotations = ToolAnnotations::new();
            annotations.read_only_hint = d.read_only_hint;
            annotations.idempotent_hint = d.idempotent_hint;
            Ok(Tool::new(d.name.clone(), d.description.clone(), schema).annotate(annotations))
        })
        .collect()
}

#[cfg(test)]
mod tests;
