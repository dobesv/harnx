use crate::config::Config;
use crate::server_identity::ServerIdentity;
use crate::tool_selector::ToolSelector;
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use futures_util::TryStreamExt;
use harnx_core::abort::{wait_abort_signal, AbortSignal};
use harnx_core::execution_context::{ToolObservationProvenance, EXECUTION_CONTEXT_NAMESPACE};
use harnx_core::instance::{ServerScope, HARNX_SERVER_SCOPE};
use harnx_core::tool::{
    JsonSchema, ToolCallOrigin, ToolDeclaration, ToolError, ToolProgress, ToolProvider,
    ToolProviderOutput,
};
use harnx_toolset::{
    registration_kv_key, user_key_segment, ControlMessage, Registration, RegistrationAudience,
    ToolErrorPayload, ToolReply, ToolRequest, ToolSpec, CAPABILITY_NEW_MESSAGE_PENDING,
    CAPABILITY_STRUCTURED_RESULT, CAPABILITY_TOOL_PROGRESS, HDR_CALL_ID, HDR_CONTENT_TYPE,
    HDR_IDEMPOTENCY_KEY, HDR_INSTANCE_ID,
};
use harnx_toolset_server::TOOL_REGISTRY_BUCKET;
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
use std::time::Duration;
use uuid::Uuid;

mod cancellation;
#[cfg(test)]
#[path = "nats_tool_provider/cancellation_races.rs"]
mod cancellation_races;
mod in_flight;
pub use cancellation::publish_tool_cancel;
use in_flight::InFlightFailure;
pub(crate) use in_flight::InFlightRegistration;
pub use in_flight::{InFlightCancelTarget, NatsInFlightCalls};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhysicalInvocationScope {
    pub physical_id: String,
    pub tool_call_id: String,
}

tokio::task_local! {
    static PHYSICAL_INVOCATION: PhysicalInvocationScope;
}

/// Execute `future` within the scope of a runtime-issued physical invocation ID
/// bound to a specific transcript `tool_call_id`.
///
/// When active, [`NatsToolProvider`] uses `physical_id` as the wire call UUID (`call_id`)
/// only for downstream tool requests whose `origin.tool_call_id` matches `tool_call_id`.
/// Nested calls with different tool call IDs (e.g. inner calls inside bash) generate
/// fresh random UUIDs.
pub async fn with_physical_invocation<F: std::future::Future>(
    physical_id: impl Into<String>,
    tool_call_id: impl Into<String>,
    future: F,
) -> F::Output {
    PHYSICAL_INVOCATION
        .scope(
            PhysicalInvocationScope {
                physical_id: physical_id.into(),
                tool_call_id: tool_call_id.into(),
            },
            future,
        )
        .await
}

/// Read the ambient physical invocation ID if one is active and matches `tool_call_id`.
pub fn current_physical_invocation_for(tool_call_id: &str) -> Option<String> {
    PHYSICAL_INVOCATION
        .try_with(|scope| {
            if scope.tool_call_id == tool_call_id {
                Some(scope.physical_id.clone())
            } else {
                None
            }
        })
        .ok()
        .flatten()
}

/// Read the current ambient physical invocation scope, if set.
// qual:api — Read by diagnostics to inspect the active physical scope.
pub fn current_physical_invocation() -> Option<PhysicalInvocationScope> {
    PHYSICAL_INVOCATION.try_with(Clone::clone).ok()
}
mod new_message;
#[cfg(test)]
pub(crate) use new_message::NEW_MESSAGE_ATTEMPTS;
mod execution_context;
#[cfg(test)]
#[path = "nats_tool_provider/new_message_tests.rs"]
mod new_message_tests;
mod partial_result;
mod progress;
mod replay;
mod request;
pub(crate) use replay::decode_journaled_reply;

use execution_context::extract_execution_context;
use progress::ProgressDispatcher;

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const REGISTRATION_LIVENESS_CHECK_INTERVAL: Duration = Duration::from_secs(1);
const JSON_CONTENT_TYPE: &str = "application/json";

#[derive(Clone, Debug)]
#[cfg_attr(test, derive(Default))]
struct RegisteredTool {
    server: String,
    selector_server: String,
    raw_name: String,
    request_timeout: Option<Duration>,
    /// The tool ends its wait when a new message arrives for the session.
    stop_on_new_message: bool,
    /// The tool declared `harnx:input_required`, so the runtime opens the
    /// durable interaction path for rounds containing it even with no
    /// middleware configured (T4.3).
    requests_input: bool,
    /// The KV key this tool's registration was discovered under, so liveness
    /// checks watch the same entry dispatch resolved (a user-scoped tool's
    /// registration lives under `{scope}.{identity}.u.{U}`, not the server's
    /// global key).
    registration_key: String,
    /// Which sessions the registration is visible to (invariant (b): dispatch
    /// re-checks even though discovery already filtered by session owner).
    audience: RegistrationAudience,
    /// How the tool's structured result reads inside a `bash` script.
    script_output: Option<harnx_toolset::ScriptOutput>,
    /// JSON Schema of the tool's structured result.
    output_schema: Option<serde_json::Value>,
}

impl RegisteredTool {
    /// Invariant (b) of the user-credentials plan: a user-scoped tool may only
    /// dispatch for its own owner, an ownerless-only tool only for a session
    /// without an owner. Global registrations dispatch for anyone.
    fn visible_to(&self, session_owner: Option<&str>) -> bool {
        match &self.audience {
            RegistrationAudience::Global => true,
            RegistrationAudience::User(segment) => {
                session_owner.is_some_and(|owner| user_key_segment(owner) == *segment)
            }
            RegistrationAudience::OwnerlessOnly => session_owner.is_none(),
        }
    }
}

struct PendingToolRequest {
    durable: ToolRequest,
    call_id: String,
    server: String,
    stop_on_new_message: bool,
    registration_key: String,
    subject: String,
    request: async_nats::Request,
}

/// Core-NATS tool provider built from one turn's KV registration snapshot.
pub struct NatsToolProvider {
    run_context: Option<crate::nats_session_metadata::RunLimitsRecord>,
    client: async_nats::Client,
    instance_id: ServerScope,
    parent_session_id: Option<String>,
    /// Caller's agent name for cid: URL construction.
    parent_agent: Option<String>,
    /// Caller's local session id for cid: URL construction.
    parent_local_session_id: Option<String>,
    /// The session owner this snapshot was discovered for (the canonical
    /// `UserPrincipals.user_id`, `None` for an ownerless session). Every
    /// dispatch through this provider stamps it onto `ToolRequest.user_id`
    /// and guards audience visibility against it. For operator calls the
    /// worker overrides the config's principals to the operator identity
    /// before discovery, so this carries the operator, not the session
    /// owner (decision 11).
    session_owner: Option<String>,
    tools: HashMap<String, RegisteredTool>,
    registrations: Vec<Registration>,
    active_package: Option<String>,
    declarations: Vec<ToolDeclaration>,
    registry: Option<async_nats::jetstream::kv::Store>,
    /// Durability for the invocation journal's bucket. Whichever writer
    /// creates the bucket first fixes its replica count for every later one,
    /// and dispatch is usually first, so it has to carry the cluster's
    /// configured count rather than assume a single replica.
    journal_replicas: usize,
    /// The invocation journal, opened by the first call that needs it.
    journal: tokio::sync::OnceCell<harnx_toolset_server::invocation_journal::InvocationJournal>,
    // Owns the flushed control subscription and its call-id progress routes.
    progress_dispatcher: ProgressDispatcher,
    in_flight: NatsInFlightCalls,
}

fn resolve_parent_identity(config: &Config) -> (Option<String>, Option<String>, Option<String>) {
    let Some(session) = config.session.as_ref() else {
        return (None, None, None);
    };
    let parent_agent = session
        .agent_name()
        .filter(|name| !name.is_empty() && *name != harnx_core::agent_config::TEMP_AGENT_NAME)
        .map(str::to_string);
    (
        Some(session.storage_key()),
        parent_agent,
        Some(session.id.clone()),
    )
}

enum DiscoveryPolicy<'a> {
    BestEffort(Option<&'a str>),
    Strict(Option<&'a str>),
}

/// The registry revision evidence a discovery scan captured.
///
/// `Sequence(0)` is a proof, not an absence: the stream did not exist when
/// the scan started. `Unread` means the pre-scan revision read failed and
/// the scan's snapshot carries no revision proof — the discovery cache must
/// not store a fabricated 0 in its place, or a later bucket deletion would
/// probe `Some(0)`, match, and serve removed tools until the TTL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RegistryRevision {
    /// The registry stream's sequence when the scan started.
    Sequence(u64),
    /// The pre-scan revision read failed; no proof exists.
    Unread,
}

/// Upper bound on a cached lookup's freshness probe. A partitioned broker
/// must not stall every completion for the client's default 10 s request
/// timeout; the probe degrades to the watch-epoch fallback after this.
const REGISTRY_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

impl<'a> DiscoveryPolicy<'a> {
    /// The package the snapshot is scoped to, and whether discovery must
    /// fail (`Strict`) instead of degrading when the registry is not
    /// authoritative for it.
    fn parts(self) -> (Option<&'a str>, bool) {
        match self {
            DiscoveryPolicy::BestEffort(package) => (package, false),
            DiscoveryPolicy::Strict(package) => (package, true),
        }
    }
}

async fn discover_registrations(
    jetstream: &async_nats::jetstream::Context,
    instance_id: &ServerScope,
    session_owner: Option<&str>,
    strict: bool,
) -> anyhow::Result<(Option<async_nats::jetstream::kv::Store>, Vec<Registration>)> {
    match open_registry_store(jetstream).await {
        Ok(Some(store)) => {
            match registration_snapshot_in(&store, instance_id, session_owner).await {
                Ok(registrations) => Ok((Some(store), registrations)),
                Err(error) => {
                    if strict {
                        return Err(error);
                    }
                    log::warn!(
                        "tool registration discovery failed under scope '{}': {error:#}",
                        instance_id.as_str()
                    );
                    Ok((Some(store), Vec::new()))
                }
            }
        }
        Ok(None) => Ok((None, Vec::new())),
        Err(error) => {
            if strict {
                return Err(error);
            }
            // An absent scope is valid; a failed KV scan must still be visible.
            log::warn!(
                "tool registration discovery failed under scope '{}': {error:#}",
                instance_id.as_str()
            );
            Ok((None, Vec::new()))
        }
    }
}

/// Boxed-erased future from [`NatsToolProvider::open_discovery_transport`]:
/// the broker connect chain must not appear as a concrete type in callers'
/// future trees (E0275 in dependent crates).
type DiscoveryTransportFuture<'a> = std::pin::Pin<
    Box<
        dyn std::future::Future<
                Output = anyhow::Result<(async_nats::Client, async_nats::Subscriber, usize)>,
            > + Send
            + 'a,
    >,
>;

impl NatsToolProvider {
    /// Dispatch one trusted frontend request with its already-persisted outer scope.
    /// Keep connection catalogs reusable without sharing a deadline between requests.
    pub async fn call_tool_with_external_admission(
        &self,
        tool_name: &str,
        arguments: Value,
        tool_call_id: &str,
        record: &crate::nats_session_metadata::RunLimitsRecord,
        abort: &AbortSignal,
    ) -> Result<ToolProviderOutput, ToolError> {
        if self.run_context.is_some() || record.parent_invocation.is_some() {
            return Err(ToolError::Fatal(anyhow!(
                "external tool admission cannot replace an inherited run"
            )));
        }
        Box::pin(self.call_registered_tool(
            request::ToolCallInput {
                name: tool_name,
                arguments,
                origin: ToolCallOrigin {
                    structured_result: false,
                    tool_call_id: Some(tool_call_id),
                    tool_round: None,
                },
                run_context: Some(record),
            },
            abort,
            None,
        ))
        .await
    }

    /// Connect through runtime config and snapshot registered tools for this instance.
    pub async fn discover(
        config: &Config,
        instance_id: ServerScope,
        in_flight: NatsInFlightCalls,
        active_package: Option<&str>,
    ) -> anyhow::Result<Self> {
        Ok(
            Self::discover_with_registry_revision(config, instance_id, in_flight, active_package)
                .await?
                .0,
        )
    }

    /// Operator discovery must distinguish an empty catalog from a failed scan.
    pub async fn discover_strict(
        config: &Config,
        instance_id: ServerScope,
        in_flight: NatsInFlightCalls,
        active_package: Option<&str>,
    ) -> anyhow::Result<Self> {
        let policy = DiscoveryPolicy::Strict(active_package);
        Self::discover_inner(config, instance_id, in_flight, policy)
            .await
            .map(|(provider, _)| provider)
    }

    /// Like [`Self::discover`], but also returns the registry revision
    /// evidence the scan captured (decision 5b), so the discovery cache can
    /// prove freshness without a second connection: both the revision read
    /// and the scan run on this discovery's own transport.
    pub(crate) async fn discover_with_registry_revision(
        config: &Config,
        instance_id: ServerScope,
        in_flight: NatsInFlightCalls,
        active_package: Option<&str>,
    ) -> anyhow::Result<(Self, RegistryRevision)> {
        let policy = DiscoveryPolicy::BestEffort(active_package);
        Self::discover_inner(config, instance_id, in_flight, policy).await
    }

    /// The registry stream sequence as seen through this snapshot's own
    /// connection, or `None` when the probe cannot reach the broker. Used by
    /// the discovery cache's freshness check: probing through the snapshot's
    /// client avoids the per-call connection cost of `Config::nats_client`
    /// (PR #2835 macOS CI latency).
    ///
    /// A missing registry stream is sequence 0, not a failure: deleting the
    /// whole bucket must invalidate a snapshot cached with a later revision
    /// (decision 5b), or the removed tools stay advertised until the TTL.
    /// Only a transport/API error maps to `None`, where the watch epoch
    /// alone vouches for the snapshot — the pre-change best-effort behavior.
    // Type-erased box for the same reason as the discovery transport: the
    // JetStream get_stream chain must not appear as a concrete type in
    // dependent crates' future trees.
    pub(crate) fn registry_revision_probe(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<u64>> + Send + '_>> {
        // Bound the probe's reply wait: a partitioned broker must not stall
        // every cached lookup for the client's default 10 s request timeout.
        let jetstream = async_nats::jetstream::context::ContextBuilder::new()
            .timeout(REGISTRY_PROBE_TIMEOUT)
            .build(self.client.clone());
        Box::pin(async move {
            match crate::tool_registry_watch::registry_state_revision(&jetstream).await {
                Ok(revision) => Some(revision.unwrap_or(0)),
                Err(error) => {
                    log::debug!("tool registry freshness check failed: {error:#}");
                    None
                }
            }
        })
    }

    async fn discover_inner(
        config: &Config,
        instance_id: ServerScope,
        in_flight: NatsInFlightCalls,
        policy: DiscoveryPolicy<'_>,
    ) -> anyhow::Result<(Self, RegistryRevision)> {
        let (active_package, strict) = policy.parts();
        let (client, control_subscription, journal_replicas) =
            Self::open_discovery_transport(config, &instance_id).await?;

        // Trusted identity for this snapshot: the session owner (or the
        // operator identity the worker stamped for an operator call). Never
        // derived from model arguments.
        let session_owner = config
            .extract_agent()
            .user_principals()
            .and_then(|principals| principals.user_id.clone())
            .filter(|owner| !owner.trim().is_empty());

        let jetstream = async_nats::jetstream::new(client.clone());
        // Decision 5b: read the registry sequence *before* the scan, on this
        // same connection (`scan_start_revision` — a write the broker acked
        // before that read is in both the scan and the sequence; a write
        // acked after it bumps the sequence past the stored value, so the
        // next completion's comparison catches it and rescans).
        let revision = scan_start_revision(&jetstream).await;
        let (registry, registrations) =
            discover_registrations(&jetstream, &instance_id, session_owner.as_deref(), strict)
                .await?;
        let registrations = ordered_registrations(registrations, active_package)?;
        let (tools, declarations) =
            build_registered_tools(&instance_id, active_package, registrations.clone());
        let (parent_session_id, parent_agent, parent_local_session_id) =
            resolve_parent_identity(config);

        Ok((
            Self {
                run_context: config.run_context.clone(),
                client,
                instance_id,
                parent_session_id,
                parent_agent,
                parent_local_session_id,
                session_owner,
                tools,
                registrations,
                active_package: active_package.map(str::to_string),
                declarations,
                registry,
                journal_replicas,
                journal: Default::default(),
                progress_dispatcher: ProgressDispatcher::new(control_subscription),
                in_flight,
            },
            revision,
        ))
    }

    /// The broker pieces a discovery snapshot needs: the shared client, the
    /// subscribed control channel for progress and cancellation, and the
    /// replica count the snapshot's journal writes must reach.
    // Type-erased box: resolving the broker (possibly starting the shared
    // local server) and connecting reach the deep async-nats/rustls future
    // chain, and a concrete boxed future would still overflow dependent
    // crates' trait-solver recursion budget (E0275).
    fn open_discovery_transport<'a>(
        config: &'a Config,
        instance_id: &'a ServerScope,
    ) -> DiscoveryTransportFuture<'a> {
        Box::pin(async move {
            let cluster = config.default_cluster_key();
            let journal_replicas = config
                .resolve_nats_server(cluster)
                .await?
                .resolved_replicas();
            let client = config.nats_client(cluster).await?;
            let control_subscription = client.subscribe(instance_id.control_subject()).await?;
            client.flush().await?;
            Ok((client, control_subscription, journal_replicas))
        })
    }

    fn resolve_route(&self, tool_name: &str) -> Option<RegisteredTool> {
        if let Some((identity_token, raw_name)) = ServerIdentity::parse_agent_tool_name(
            tool_name,
            &self.registrations,
            self.active_package.as_deref(),
        ) {
            // Union semantics (decision 4): one identity can carry a global
            // registration plus one in this session's audience. Pick the
            // registration whose tools contain the raw name — not just the
            // first registration with the identity — so KV iteration order
            // never decides the result. `self.registrations` is ordered by
            // [`registration_order`], so ties resolve deterministically.
            let registration = self.registrations.iter().find(|registration| {
                ServerIdentity::identity_token(registration) == identity_token
                    && registration.tools.iter().any(|spec| spec.name == raw_name)
            })?;
            let spec = registration
                .tools
                .iter()
                .find(|spec| spec.name == raw_name)?;
            return Some(RegisteredTool {
                server: identity_token,
                selector_server: registration.server.clone(),
                raw_name,
                request_timeout: request_timeout(spec.timeout_secs),
                stop_on_new_message: spec.stops_on_new_message(),
                requests_input: spec.requests_input(),
                registration_key: registration_kv_key(self.instance_id.as_str(), registration),
                audience: registration.audience(),
                script_output: spec.script_output(),
                output_schema: spec.output_schema().cloned(),
            });
        }

        // Raw aliases keep persisted calls from before server-prefixed naming routable.
        self.tools.get(tool_name).cloned()
    }
    /// Script-facing declarations by agent-visible tool name. Each declaration
    /// resolves to its own route, so a raw-name alias never becomes a key. A
    /// tool that declares neither a mapping nor a schema with properties has
    /// no entry. The schema is handed on as its summary, so `help` reads one
    /// plain type per field however the tool wrote it (`anyOf`, `$ref`).
    pub(crate) fn script_shapes(&self) -> std::collections::BTreeMap<String, crate::ScriptShape> {
        self.declarations
            .iter()
            .filter_map(|declaration| {
                let route = self.resolve_route(&declaration.name)?;
                let shape = crate::ScriptShape {
                    output: route.script_output,
                    output_schema: route
                        .output_schema
                        .as_ref()
                        .and_then(harnx_toolset::output_schema_summary),
                };
                let declared = shape.output.is_some() || shape.output_schema.is_some();
                declared.then(|| (declaration.name.clone(), shape))
            })
            .collect()
    }

    pub fn declarations(&self) -> &[ToolDeclaration] {
        &self.declarations
    }

    /// Every name this provider can route. The route-map keys cover the
    /// agent-visible names and the raw aliases kept through the naming
    /// transition; `has_tool` additionally accepts the per-registration
    /// agent-visible name (`parse_agent_tool_name`'s bare-name fallback,
    /// `server_identity.rs`), which cross-package registrations resolve
    /// under without appearing as a map key — so yield it too. The
    /// config-level collision view records these names so the
    /// model-visible tool list cannot advertise a script tool whose name
    /// the provider serves.
    pub fn route_names(&self) -> impl Iterator<Item = String> {
        let mut names: Vec<String> = self.tools.keys().cloned().collect();
        for registration in &self.registrations {
            for spec in &registration.tools {
                names.push(ServerIdentity::agent_visible_name(
                    registration.package.as_deref(),
                    registration,
                    &spec.name,
                ));
            }
        }
        names.into_iter()
    }

    pub fn declarations_for_use_tools(&self, use_tools: Option<&str>) -> Vec<ToolDeclaration> {
        let Some(use_tools) = use_tools else {
            return Vec::new();
        };
        let selectors: Vec<(&str, ToolSelector)> =
            harnx_core::agent_config::split_tool_selectors(use_tools)
                .into_iter()
                .map(|selector| {
                    let selector = selector.trim();
                    (selector, ToolSelector::new(selector, false))
                })
                .collect();
        self.declarations
            .iter()
            .filter(|declaration| {
                // A selector can name the tool, its server, or its unprefixed name.
                let route = self.tools.get(&declaration.name);
                let names = [
                    Some(declaration.name.as_str()),
                    route.map(|route| route.selector_server.as_str()),
                    route.map(|route| route.raw_name.as_str()),
                ];
                selectors.iter().any(|(selector, compiled)| {
                    *selector == "*"
                        || names
                            .iter()
                            .flatten()
                            .any(|name| name == selector || compiled.is_match(name))
                })
            })
            .cloned()
            .collect()
    }
    /// Ordinary inference keeps its historical collision policy. Explicit calls
    /// must not silently choose one of several servers with the same visible name.
    pub fn ensure_unambiguous(
        &self,
        names: &std::collections::HashSet<String>,
    ) -> anyhow::Result<()> {
        let mut owners = HashMap::new();
        for registration in &self.registrations {
            for spec in &registration.tools {
                let name = ServerIdentity::agent_visible_name(
                    self.active_package.as_deref(),
                    registration,
                    &spec.name,
                );
                if !names.contains(&name) {
                    continue;
                }
                let owner = (
                    ServerIdentity::identity_token(registration),
                    spec.name.clone(),
                );
                if let Some(previous) = owners.insert(name.clone(), owner.clone()) {
                    anyhow::ensure!(
                        previous == owner,
                        "Tool '{name}' is ambiguous between routes {previous:?} and {owner:?}"
                    );
                }
            }
        }
        Ok(())
    }

    pub fn in_flight_calls(&self) -> NatsInFlightCalls {
        self.in_flight.clone()
    }

    /// A provider for tool-round snapshot tests: direct control over the
    /// route map (raw aliases included), the declaration list, and the
    /// registrations whose names `parse_agent_tool_name` can fall back to,
    /// without registration plumbing. Only `has_tool`, `route_names`, and
    /// `declarations_for_use_tools` consult those fields in that context.
    #[cfg(test)]
    pub(crate) async fn snapshot_test_provider(
        client: &async_nats::Client,
        routes: Vec<(&str, &str, &str)>,
        declarations: Vec<ToolDeclaration>,
        registrations: Vec<Registration>,
    ) -> anyhow::Result<NatsToolProvider> {
        let instance_id = ServerScope::new();
        let control_subscription = client.subscribe(instance_id.control_subject()).await?;
        Ok(NatsToolProvider {
            run_context: None,
            client: client.clone(),
            instance_id,
            session_owner: None,
            parent_session_id: None,
            parent_agent: None,
            parent_local_session_id: None,
            tools: routes
                .into_iter()
                .map(|(name, server, raw_name)| {
                    (
                        name.to_string(),
                        RegisteredTool {
                            server: server.into(),
                            selector_server: server.into(),
                            raw_name: raw_name.into(),
                            ..Default::default()
                        },
                    )
                })
                .collect(),
            registrations,
            active_package: None,
            declarations,
            registry: None,
            journal_replicas: 1,
            journal: Default::default(),
            progress_dispatcher: ProgressDispatcher::new(control_subscription),
            in_flight: NatsInFlightCalls::default(),
        })
    }

    /// Describe this snapshot's discovery for the zero-registration guard.
    ///
    /// Reuses the registrations already fetched by [`Self::discover`] instead
    /// of re-scanning the registry, so callers on the per-turn refresh path
    /// don't pay for another KV round trip just to log a diagnostic.
    pub fn discovery_report(&self) -> DiscoveryReport {
        let found = self.registrations.len();
        DiscoveryReport {
            found,
            message: discovery_message(&self.instance_id, found),
        }
    }

    async fn wait_for_registration_loss(&self, key: &str) -> String {
        let Some(registry) = self.registry.as_ref() else {
            std::future::pending::<()>().await;
            unreachable!("a pending future cannot complete")
        };
        let mut checks = tokio::time::interval(REGISTRATION_LIVENESS_CHECK_INTERVAL);
        checks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut failures = 0;
        loop {
            checks.tick().await;
            match tokio::time::timeout(Duration::from_secs(10), registry.get(key)).await {
                Ok(Ok(Some(_))) => failures = 0,
                Ok(Ok(None)) => {
                    return format!(
                        "tool server unavailable: registration '{key}' is no longer active"
                    );
                }
                error => {
                    failures += 1;
                    log::warn!("tool registration liveness check failed for '{key}' ({failures}/3): {error:?}");
                    if failures == 3 {
                        // This establishes loss of observability, not that the
                        // tool stopped. The caller preserves that distinction.
                        return format!("tool completion unconfirmed: NATS registration '{key}' could not be read after 3 attempts");
                    }
                }
            }
        }
    }

    async fn await_response(
        &self,
        pending: PendingToolRequest,
        abort: &AbortSignal,
    ) -> Result<async_nats::Message, ToolError> {
        let PendingToolRequest {
            call_id,
            server,
            stop_on_new_message,
            registration_key,
            subject,
            request,
            durable,
        } = pending;
        let (mut supervised_failure, completion) = self
            .in_flight
            .register_guarded(
                InFlightRegistration {
                    call_id: call_id.clone(),
                    server: server.clone(),
                    session_id: durable.parent_session_id.clone().unwrap_or_default(),
                    control_subject: self.instance_id.control_subject(),
                    stop_on_new_message,
                },
                || {
                    if !abort.aborted_failover() {
                        self.schedule_cancel(&durable, &server);
                    }
                },
            )
            .await;
        if stop_on_new_message {
            self.signal_message_since_dispatch(&durable, &server);
        }
        let request = harnx_nats_common::rpc::request(&self.client, subject, request);
        tokio::pin!(request);
        let response = tokio::select! {
            _ = wait_abort_signal(abort) => {
                Err(ToolError::Fatal(anyhow!("tool call aborted")))
            }
            failure = &mut supervised_failure => {
                let message = match failure {
                    Ok(InFlightFailure::Unavailable(message)) => message,
                    Err(_) => "tool server unavailable".to_string(),
                };
                Err(ToolError::Recoverable(anyhow!(message)))
            }
            message = self.wait_for_registration_loss(&registration_key) => {
                Err(ToolError::Recoverable(anyhow!(message)))
            }
            response = &mut request => response.map_err(|error| {
                ToolError::Recoverable(anyhow!("tool server unavailable: {error}"))
            }),
        };
        // A displaced wait must not cancel the shared call ID. Failover also
        // leaves remote work intact so a replacement worker can recover it.
        if !completion.complete() {
            return response;
        }
        if response.is_err() && !abort.aborted_failover() {
            self.send_cancel(&durable, &server).await;
        }
        response
    }
}

/// Deterministic order over one scope's registrations: by server identity
/// token, then by audience (global, ownerless-only, then user registrations
/// by user key segment). Discovery sorts with this before building the tool
/// map, so KV iteration order never decides which registration wins a
/// same-name tool (decision 4) and `resolve_route` picks are reproducible.
fn registration_order(left: &Registration, right: &Registration) -> std::cmp::Ordering {
    registration_sort_key(left).cmp(&registration_sort_key(right))
}

/// Reject registrations from a different tool protocol, then order them for
/// discovery: the active package's registrations first, then the
/// deterministic identity order (decision 4, "several registrations per
/// identity" — KV iteration order must not decide which of one identity's
/// global/user/ownerless registrations wins a same-name tool).
fn ordered_registrations(
    mut registrations: Vec<Registration>,
    active_package: Option<&str>,
) -> anyhow::Result<Vec<Registration>> {
    for registration in &registrations {
        anyhow::ensure!(
            registration.proto_version == harnx_toolset_server::TOOL_PROTOCOL_VERSION,
            "incompatible tool protocol {} from '{}'; expected v{}; upgrade workers and tool servers together",
            registration.proto_version,
            registration.server,
            harnx_toolset_server::TOOL_PROTOCOL_VERSION
        );
    }
    registrations.sort_by_key(|registration| match registration.package.as_deref() {
        Some(package) if Some(package) == active_package => 0,
        None => usize::from(active_package.is_some()),
        Some(_) => 2,
    });
    registrations.sort_by(registration_order);
    Ok(registrations)
}

/// The comparison key for [`registration_order`]: identity token first,
/// then the audience rank.
fn registration_sort_key(registration: &Registration) -> (String, (u8, String)) {
    (
        ServerIdentity::identity_token(registration),
        audience_rank(registration),
    )
}

/// Audience rank: global before ownerless-only before user registrations,
/// users ordered by their key segment.
fn audience_rank(registration: &Registration) -> (u8, String) {
    match registration.audience() {
        RegistrationAudience::Global => (0, String::new()),
        RegistrationAudience::OwnerlessOnly => (1, String::new()),
        RegistrationAudience::User(segment) => (2, segment),
    }
}

fn registered_tool(
    scope: &ServerScope,
    active_package: Option<&str>,
    registration: &Registration,
    spec: ToolSpec,
) -> Option<(String, RegisteredTool, ToolDeclaration)> {
    let template = |key| {
        spec.meta
            .as_ref()
            .and_then(|meta| meta.get(key))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let call_template = template("call_template");
    let result_template = template("result_template");
    // Extract kind before moving spec
    let kind = spec.kind().map(|k| k.into());
    let stop_on_new_message = spec.stops_on_new_message();
    let requests_input = spec.requests_input();
    let script_output = spec.script_output();
    let output_schema = spec.output_schema().cloned();
    let parameters = match JsonSchema::from_tool_schema(spec.input_schema) {
        Ok(parameters) => parameters,
        Err(error) => {
            log::warn!(
                "ignoring invalid schema for NATS tool '{}.{}': {error}",
                registration.server,
                spec.name
            );
            return None;
        }
    };
    let raw_name = spec.name.clone();
    let name = ServerIdentity::agent_visible_name(active_package, registration, &raw_name);
    let route = RegisteredTool {
        server: ServerIdentity::identity_token(registration),
        selector_server: registration.server.clone(),
        raw_name,
        request_timeout: request_timeout(spec.timeout_secs),
        stop_on_new_message,
        requests_input,
        registration_key: registration_kv_key(scope.as_str(), registration),
        audience: registration.audience(),
        script_output,
        output_schema,
    };
    let declaration = ToolDeclaration {
        name: name.clone(),
        description: spec.description,
        parameters,
        mcp_tool_name: None,
        mcp_server_name: None,
        call_template,
        result_template,
        idempotent_hint: Some(spec.idempotent_hint),
        read_only_hint: Some(spec.read_only_hint),
        kind,
    };
    Some((name, route, declaration))
}

fn request_timeout(timeout_secs: Option<u64>) -> Option<Duration> {
    match timeout_secs {
        Some(0) => None,
        Some(seconds) => Some(Duration::from_secs(seconds)),
        None => Some(DEFAULT_REQUEST_TIMEOUT),
    }
}
fn build_registered_tools(
    scope: &ServerScope,
    active_package: Option<&str>,
    mut registrations: Vec<Registration>,
) -> (HashMap<String, RegisteredTool>, Vec<ToolDeclaration>) {
    registrations.sort_by(registration_order);
    let mut registered = HashMap::new();
    for registration in registrations {
        for spec in registration.tools.clone() {
            let Some((name, route, declaration)) =
                registered_tool(scope, active_package, &registration, spec)
            else {
                continue;
            };
            if let Some((previous, _)) = registered.insert(name.clone(), (route, declaration)) {
                log::warn!(
                    "duplicate NATS tool '{name}' from server '{}'; server '{}' wins",
                    previous.server,
                    registration.server
                );
            }
        }
    }
    let mut entries = registered.into_iter().collect::<Vec<_>>();
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    let mut tools = HashMap::new();
    let mut declarations = Vec::with_capacity(entries.len());
    for (name, (route, declaration)) in entries {
        // Keep raw aliases during the naming transition so injected and persisted
        // calls from older turns still route to the same server.
        tools.insert(route.raw_name.clone(), route.clone());
        tools.insert(name, route);
        declarations.push(declaration);
    }
    (tools, declarations)
}

#[async_trait]
impl ToolProvider for NatsToolProvider {
    async fn replay_tool_call(
        &self,
        replay: harnx_core::tool::ToolReplay<'_>,
        abort: &AbortSignal,
    ) -> Result<Option<ToolProviderOutput>, ToolError> {
        Box::pin(self.replay_recorded_call(replay, abort))
            .await
            .map_err(ToolError::Fatal)
    }

    fn name(&self) -> &str {
        "nats"
    }

    fn has_tool(&self, tool_name: &str) -> bool {
        self.resolve_route(tool_name).is_some()
    }

    fn may_request_input(&self, tool_name: &str) -> bool {
        self.resolve_route(tool_name)
            .is_some_and(|route| route.requests_input)
    }

    async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Value,
        abort: &AbortSignal,
    ) -> Result<ToolProviderOutput, ToolError> {
        self.call_tool_with_id(tool_name, arguments, None, abort)
            .await
    }

    async fn call_tool_with_id(
        &self,
        tool_name: &str,
        arguments: Value,
        tool_call_id: Option<&str>,
        abort: &AbortSignal,
    ) -> Result<ToolProviderOutput, ToolError> {
        Box::pin(self.call_registered_tool(
            request::ToolCallInput {
                name: tool_name,
                arguments,
                origin: ToolCallOrigin {
                    structured_result: false,
                    tool_call_id,
                    tool_round: None,
                },
                run_context: None,
            },
            abort,
            None,
        ))
        .await
    }

    async fn call_tool_with_progress(
        &self,
        tool_name: &str,
        arguments: Value,
        origin: ToolCallOrigin<'_>,
        abort: &AbortSignal,
        progress: std::sync::Arc<dyn ToolProgress>,
    ) -> Result<ToolProviderOutput, ToolError> {
        Box::pin(self.call_registered_tool(
            request::ToolCallInput {
                name: tool_name,
                arguments,
                origin,
                run_context: None,
            },
            abort,
            Some(progress),
        ))
        .await
    }
}

/// The registry sequence a scan starts under, or why none exists.
///
/// Read strictly before the scan on the same connection (decision 5b).
/// A failed read is not a fabricated sequence: the scan's snapshot must
/// carry no revision proof, or a later bucket deletion (probe `Some(0)`)
/// would match a cached 0 and stay stale until the TTL.
async fn scan_start_revision(jetstream: &async_nats::jetstream::Context) -> RegistryRevision {
    match crate::tool_registry_watch::registry_state_revision(jetstream).await {
        Ok(sequence) => RegistryRevision::Sequence(sequence.unwrap_or(0)),
        Err(error) => {
            log::debug!("tool registry revision read failed before scan: {error:#}");
            RegistryRevision::Unread
        }
    }
}

/// Open the tool registry bucket, treating "the stream genuinely doesn't
/// exist yet" as absent rather than an error — no tool server has ever
/// registered against this cluster, which the caller reports as zero
/// registrations, not a failure.
///
/// `get_key_value` wraps *every* `get_stream` failure — a broker outage, a
/// permissions error, a timeout, not just "not found" — in
/// `KeyValueErrorKind::GetBucket`. Matching on that kind alone (as this used
/// to) turns any of those into "zero tools registered", silently, which is
/// the exact failure class the zero-registration warning exists to catch.
/// Check the stream directly first and match the specific JetStream
/// STREAM_NOT_FOUND error code instead. Other failures must remain visible.
async fn open_registry_store(
    jetstream: &async_nats::jetstream::Context,
) -> anyhow::Result<Option<async_nats::jetstream::kv::Store>> {
    use async_nats::jetstream::{context::GetStreamErrorKind, ErrorCode};
    match jetstream
        .get_stream(format!("KV_{TOOL_REGISTRY_BUCKET}"))
        .await
    {
        Ok(_) => Ok(Some(jetstream.get_key_value(TOOL_REGISTRY_BUCKET).await?)),
        Err(error)
            if matches!(
                error.kind(),
                GetStreamErrorKind::JetStream(ref error)
                    if error.kind() == ErrorCode::STREAM_NOT_FOUND
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error.into()),
    }
}

async fn registration_snapshot(
    client: &async_nats::Client,
    instance_id: &ServerScope,
    session_owner: Option<&str>,
) -> anyhow::Result<Vec<Registration>> {
    let jetstream = async_nats::jetstream::new(client.clone());
    let Some(store) = open_registry_store(&jetstream).await? else {
        return Ok(Vec::new());
    };
    registration_snapshot_in(&store, instance_id, session_owner).await
}

/// Whether one discovered registration is visible to a session with this
/// owner (invariant (a) of the user-credentials plan).
fn registration_visible_to(registration: &Registration, session_owner: Option<&str>) -> bool {
    match registration.audience() {
        RegistrationAudience::Global => true,
        RegistrationAudience::User(segment) => {
            session_owner.is_some_and(|owner| user_key_segment(owner) == segment)
        }
        RegistrationAudience::OwnerlessOnly => session_owner.is_none(),
    }
}

async fn registration_snapshot_in(
    store: &async_nats::jetstream::kv::Store,
    instance_id: &ServerScope,
    session_owner: Option<&str>,
) -> anyhow::Result<Vec<Registration>> {
    let mut keys = store.keys().await?;
    let prefix = format!("{instance_id}.");
    let mut registrations = Vec::new();
    while let Some(key) = keys.try_next().await? {
        if !key.starts_with(&prefix) {
            continue;
        }
        let Some(value) = store.get(&key).await? else {
            continue;
        };
        let registration = match serde_json::from_slice::<Registration>(&value) {
            Ok(registration) => registration,
            Err(error) => {
                log::warn!("ignoring invalid NATS tool registration '{key}': {error}");
                continue;
            }
        };
        // The key must be exactly the one this registration's identity and
        // audience publish under. The old rule accepted only the bare
        // `{scope}.{identity_token}` shape, which is still the only one a
        // pre-change worker sees; `.u.{U}` and `.anon` keys are new, and any
        // other suffix stays ignored.
        let expected_key = registration_kv_key(instance_id.as_str(), &registration);
        if key != expected_key {
            log::warn!(
                "ignoring NATS tool registration '{key}' filed under the wrong \
                 audience key '{expected_key}'"
            );
            continue;
        }
        // Audience filter (invariant (a)): a session with an owner sees global
        // registrations plus its own; an ownerless session sees global plus
        // ownerless-only. Another user's or an ownerless-only registration
        // is simply out of this session's visible set — not a warning.
        if !registration_visible_to(&registration, session_owner) {
            continue;
        }
        registrations.push(registration);
    }
    Ok(registrations)
}

/// What a discovery pass found, so a zero result can explain itself.
///
/// A worker that finds no tool servers under its scope looks identical to a
/// deployment with none configured; this makes the difference visible to
/// whoever is watching the logs.
pub struct DiscoveryReport {
    pub found: usize,
    pub message: String,
}

fn discovery_message(scope: &ServerScope, found: usize) -> String {
    if found == 0 {
        format!(
            "no tool servers are registered under scope '{}'; the model will \
             see built-in tools only. Check that the servers carry the same \
             {HARNX_SERVER_SCOPE} value.",
            scope.as_str()
        )
    } else {
        format!(
            "discovered {found} tool server(s) under scope '{}'",
            scope.as_str()
        )
    }
}

/// Scan the registry for `scope` and describe the result.
///
/// Runs its own prefix scan rather than reusing a cached [`NatsToolProvider`],
/// so it also works as a standalone check against a scope no provider has
/// discovered yet.
pub async fn describe_discovery(
    client: &async_nats::Client,
    scope: &ServerScope,
) -> anyhow::Result<DiscoveryReport> {
    let registrations = registration_snapshot(client, scope, None).await?;
    let found = registrations.len();
    Ok(DiscoveryReport {
        found,
        message: discovery_message(scope, found),
    })
}

#[cfg(test)]
mod tests {

    /// A global registration whose only varying fields are `server` and
    /// `tools`; every other wire field keeps its default shape.
    fn global_registration(server: &str, tools: Vec<ToolSpec>) -> Registration {
        Registration {
            user_id: None,
            ownerless_only: false,
            connection_id: None,
            package: None,
            config: String::new(),
            server: server.to_string(),
            tools,
            schema_version: 1,
            proto_version: harnx_toolset_server::TOOL_PROTOCOL_VERSION,
        }
    }
    use super::{
        build_registered_tools, registration_order, request_timeout, with_physical_invocation,
        NatsInFlightCalls, NatsToolProvider, ProgressDispatcher, RegisteredTool,
        DEFAULT_REQUEST_TIMEOUT,
    };
    use harnx_core::instance::ServerScope;
    use harnx_toolset::{Registration, ToolSpec, HDR_CALL_ID, HDR_IDEMPOTENCY_KEY};

    /// One identity's global registration plus its user-scoped one, with a
    /// same-name tool in both, in both possible KV iteration orders. Decision
    /// 4: which registration a name routes to must not depend on that order,
    /// and a same-name conflict resolves to the global registration.
    #[test]
    fn union_routing_is_independent_of_kv_iteration_order() {
        fn spec(name: &str) -> ToolSpec {
            ToolSpec {
                cancellation_guarantee: Default::default(),
                name: name.to_string(),
                description: String::new(),
                input_schema: json!({ "type": "object" }),
                idempotent_hint: true,
                read_only_hint: true,
                timeout_secs: Some(30),
                meta: None,
            }
        }
        fn registration(user_id: Option<&str>, tools: &[&str]) -> Registration {
            Registration {
                package: None,
                config: String::new(),
                server: "creds".to_string(),
                tools: tools.iter().map(|name| spec(name)).collect(),
                schema_version: 1,
                proto_version: harnx_toolset_server::TOOL_PROTOCOL_VERSION,
                user_id: user_id.map(str::to_string),
                ownerless_only: false,
                connection_id: None,
            }
        }

        let scope = ServerScope::new();
        let global = registration(None, &["manage"]);
        let user = registration(Some("alice"), &["personal"]);
        // Whichever order the KV scan yields the two registrations in, the
        // sort inside `build_registered_tools` and `registration_order` puts
        // them in the same deterministic order, so the routes agree.
        let mut global_first = vec![global.clone(), user.clone()];
        let mut user_first = vec![user.clone(), global.clone()];
        let (tools_global_first, declarations_global_first) =
            build_registered_tools(&scope, None, std::mem::take(&mut global_first));
        let (tools_user_first, declarations_user_first) =
            build_registered_tools(&scope, None, std::mem::take(&mut user_first));

        // The union is present either way: global and user tools side by side,
        // each routed to its own registration.
        let route = |tools: &HashMap<String, RegisteredTool>, name: &str| {
            tools
                .get(name)
                .unwrap_or_else(|| panic!("{name} missing"))
                .audience
                .clone()
        };
        for tools in [&tools_global_first, &tools_user_first] {
            assert!(
                tools.contains_key("creds_manage"),
                "management tool missing"
            );
            assert_eq!(
                route(tools, "creds_manage"),
                harnx_toolset::RegistrationAudience::Global
            );
            assert_eq!(
                route(tools, "creds_personal"),
                harnx_toolset::RegistrationAudience::User(harnx_toolset::user_key_segment("alice"))
            );
        }
        // And the whole snapshots agree, not just the routed entries.
        let summarize = |tools: &HashMap<String, RegisteredTool>| {
            tools
                .iter()
                .map(|(name, route)| (name.clone(), route.registration_key.clone()))
                .collect::<Vec<_>>()
                .into_iter()
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        assert_eq!(summarize(&tools_global_first), summarize(&tools_user_first));
        assert_eq!(
            declarations_global_first
                .iter()
                .map(|declaration| declaration.name.clone())
                .collect::<Vec<_>>(),
            declarations_user_first
                .iter()
                .map(|declaration| declaration.name.clone())
                .collect::<Vec<_>>()
        );

        // `registration_order` itself ranks global before user for one
        // identity, from either input order.
        let mut user_leading = [user.clone(), global.clone()];
        user_leading.sort_by(registration_order);
        assert_eq!(
            user_leading[0].audience(),
            harnx_toolset::RegistrationAudience::Global
        );
        let mut global_leading = [global, user];
        global_leading.sort_by(registration_order);
        assert_eq!(
            global_leading[0].audience(),
            harnx_toolset::RegistrationAudience::Global
        );
    }

    use opentelemetry::global;
    use opentelemetry::trace::{TraceContextExt, TracerProvider as _};
    use opentelemetry_sdk::propagation::TraceContextPropagator;
    use opentelemetry_sdk::trace::SdkTracerProvider;
    use serde_json::json;
    use std::collections::HashMap;
    use std::time::Duration;
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    use tracing_subscriber::layer::SubscriberExt;
    use uuid::Uuid;

    /// A dispatch of the transcript call `id`, made outside any recorded round.
    fn transcript_call(id: &str) -> harnx_core::tool::ToolCallOrigin<'_> {
        harnx_core::tool::ToolCallOrigin {
            structured_result: false,
            tool_call_id: Some(id),
            tool_round: None,
        }
    }

    pub(super) async fn make_test_provider(client: &async_nats::Client) -> NatsToolProvider {
        let instance_id = ServerScope::new();
        let control_subscription = client
            .subscribe(instance_id.control_subject())
            .await
            .expect("subscribe to control subject");
        NatsToolProvider {
            client: client.clone(),
            instance_id,
            parent_session_id: None,
            run_context: None,
            parent_agent: None,
            parent_local_session_id: None,
            session_owner: None,
            tools: HashMap::new(),
            registrations: Vec::new(),
            active_package: None,
            declarations: Vec::new(),
            registry: None,
            journal_replicas: 1,
            journal: Default::default(),
            progress_dispatcher: ProgressDispatcher::new(control_subscription),
            in_flight: NatsInFlightCalls::default(),
        }
    }

    #[tokio::test]
    async fn external_tool_admission_rejects_expiry_and_cannot_replace_inherited_scope(
    ) -> anyhow::Result<()> {
        use crate::nats_session_metadata::{
            CallTimeoutOverride, RunLimitsRecord, SessionMetadataStore,
        };
        let server = crate::nats_test_common::spawn_nats_server()
            .await?
            .ok_or_else(|| anyhow::anyhow!("nats-server required"))?;
        let client = async_nats::connect(server.url()).await?;
        let js = async_nats::jetstream::new(client.clone());
        let store = SessionMetadataStore::ensure(&js, 1).await?;
        let expired = RunLimitsRecord::admit_root(
            Default::default(),
            Default::default(),
            chrono::Utc::now() - chrono::Duration::seconds(30),
            Default::default(),
            None,
            CallTimeoutOverride::from_optional(Some(1)),
        )?;
        store.put_run_limits("external-provider", &expired).await?;
        store
            .put_invocation_limits("external-provider", &expired)
            .await?;
        let fresh = RunLimitsRecord::admit_root(
            Default::default(),
            Default::default(),
            chrono::Utc::now(),
            Default::default(),
            None,
            CallTimeoutOverride::Omitted,
        )?;
        let mut provider = make_test_provider(&client).await;
        provider.tools.insert(
            "read".into(),
            RegisteredTool {
                server: "fs".into(),
                selector_server: "fs".into(),
                raw_name: "read".into(),
                ..Default::default()
            },
        );
        let abort = harnx_core::abort::create_abort_signal();
        let error = provider
            .call_tool_with_external_admission("read", json!({}), "expired", &expired, &abort)
            .await
            .unwrap_err();
        let harnx_core::tool::ToolError::Fatal(error) = error else {
            panic!("expired external scope wasn't fatal");
        };
        assert!(error.is::<crate::nats_session_metadata::run_limits::DeadlineExpired>());
        let message = error.to_string();
        for expected in [
            "scope: outer_run",
            "deadline:",
            expired.run_id.as_str(),
            expired.invocation_id.as_str(),
            "Do not retry",
            "Return to the user to confirm continuation",
            "No new dispatch occurred; tool output from this attempt is unavailable",
        ] {
            assert!(message.contains(expected), "{message}");
        }
        provider.run_context = Some(expired.clone());
        let error = provider
            .call_tool_with_external_admission("read", json!({}), "renew", &fresh, &abort)
            .await
            .unwrap_err();
        let harnx_core::tool::ToolError::Fatal(error) = error else {
            panic!("inherited scope replacement wasn't fatal");
        };
        assert_eq!(
            error.to_string(),
            "external tool admission cannot replace an inherited run"
        );
        assert_eq!(provider.run_context, Some(expired));
        Ok(())
    }

    async fn setup_test_provider_and_route(url: &str) -> (NatsToolProvider, RegisteredTool) {
        let client = async_nats::connect(url)
            .await
            .expect("connect to test NATS server");
        let provider = make_test_provider(&client).await;
        let route = RegisteredTool {
            server: "test-server".to_string(),
            raw_name: "test-tool".to_string(),
            request_timeout: Some(DEFAULT_REQUEST_TIMEOUT),
            ..Default::default()
        };
        (provider, route)
    }
    #[tokio::test]
    async fn with_physical_invocation_threads_id_to_prepared_request() {
        harnx_core::require_nextest();
        let Some(server) = spawn_nats_server_without_jetstream() else {
            eprintln!("skipping with_physical_invocation test: nats-server missing");
            return;
        };
        let (provider, route) = setup_test_provider_and_route(&server.url).await;

        let expected_phys_id = "phys-inv-12345";
        let target_call_id = "model-call-id";

        let pending = with_physical_invocation(expected_phys_id, target_call_id, async {
            provider
                .prepare_request(json!({ "value": 1 }), &route, Some(target_call_id))
                .ok()
                .expect("prepare request")
        })
        .await;

        assert_eq!(pending.call_id, expected_phys_id);
        assert_eq!(pending.durable.call_id, expected_phys_id);
        assert_eq!(pending.durable.operation_id, expected_phys_id);
        assert_eq!(
            pending.durable.tool_call_id.as_deref(),
            Some(target_call_id)
        );

        let headers = pending.request.headers.expect("NATS request headers");
        assert_eq!(headers.get(HDR_CALL_ID).unwrap().as_str(), expected_phys_id);
        assert_eq!(
            headers.get(HDR_IDEMPOTENCY_KEY).unwrap().as_str(),
            expected_phys_id
        );

        // Different tool call ID generates a fresh UUID
        let nested_inner_pending =
            with_physical_invocation(expected_phys_id, target_call_id, async {
                provider
                    .prepare_request(
                        json!({ "value": 1 }),
                        &route,
                        Some("different-inner-call-id"),
                    )
                    .ok()
                    .expect("prepare request")
            })
            .await;

        assert_ne!(nested_inner_pending.call_id, expected_phys_id);
        assert!(Uuid::parse_str(&nested_inner_pending.call_id).is_ok());

        // None tool call ID generates a fresh UUID
        let none_pending = with_physical_invocation(expected_phys_id, target_call_id, async {
            provider
                .prepare_request(json!({ "value": 1 }), &route, None)
                .ok()
                .expect("prepare request")
        })
        .await;

        assert_ne!(none_pending.call_id, expected_phys_id);
        assert!(Uuid::parse_str(&none_pending.call_id).is_ok());

        // Outside scope, generates fresh UUID
        let default_pending = provider
            .prepare_request(json!({ "value": 1 }), &route, Some(target_call_id))
            .ok()
            .expect("prepare request");
        assert_ne!(default_pending.call_id, expected_phys_id);
        assert!(Uuid::parse_str(&default_pending.call_id).is_ok());
    }

    #[tokio::test]
    async fn prepared_request_headers_carry_active_trace_id() {
        harnx_core::require_nextest();
        let Some(server) = spawn_nats_server_without_jetstream() else {
            eprintln!(
                "skipping prepared_request_headers_carry_active_trace_id: nats-server missing"
            );
            return;
        };
        let (provider, route) = setup_test_provider_and_route(&server.url).await;

        global::set_text_map_propagator(TraceContextPropagator::new());
        let tracer_provider = SdkTracerProvider::builder().build();
        let tracer = tracer_provider.tracer("nats-tool-provider-propagation-test");
        let subscriber =
            tracing_subscriber::registry().with(tracing_opentelemetry::layer().with_tracer(tracer));

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("nats_tool_provider_propagation_test");
            let _entered = span.enter();
            let expected_trace_id = tracing::Span::current()
                .context()
                .span()
                .span_context()
                .trace_id();
            assert_ne!(expected_trace_id, opentelemetry::trace::TraceId::INVALID);

            let pending = match provider.prepare_request(json!({ "value": 1 }), &route, None) {
                Ok(pending) => pending,
                Err(
                    harnx_core::tool::ToolError::Recoverable(error)
                    | harnx_core::tool::ToolError::Fatal(error),
                ) => {
                    panic!("prepare tool request: {error:#}")
                }
            };
            let headers = pending.request.headers.expect("NATS request headers");
            let traceparent = headers
                .get("traceparent")
                .expect("traceparent in NATS headers")
                .as_str();
            assert!(
                traceparent.starts_with(&format!("00-{expected_trace_id}-")),
                "traceparent {traceparent} did not carry trace ID {expected_trace_id}"
            );
        });
    }

    fn mapped_registration() -> Registration {
        read_registration("fs", true)
    }

    fn read_registration(server: &str, mapped: bool) -> Registration {
        let output = harnx_toolset::ScriptOutput {
            stdout: Some("text".into()),
            ..Default::default()
        };
        let mut spec = ToolSpec {
            cancellation_guarantee: Default::default(),
            name: "read".to_string(),
            description: "Read a file".to_string(),
            input_schema: json!({ "type": "object" }),
            idempotent_hint: true,
            read_only_hint: true,
            timeout_secs: None,
            meta: None,
        };
        if mapped {
            spec = spec
                .with_script_output(output)
                .with_output_schema(json!({"type": "object", "properties": {"text": {}}}));
        }
        global_registration(server, vec![spec])
    }

    #[test]
    fn registered_tool_keeps_the_script_output() {
        harnx_core::require_nextest();
        let registration = mapped_registration();
        let spec = registration.tools[0].clone();
        let scope = ServerScope::from_string("provider-tests");
        let (name, route, _) =
            super::registered_tool(&scope, None, &registration, spec).expect("registers");
        assert_eq!(name, "fs_read");
        assert_eq!(
            route.script_output.and_then(|output| output.stdout),
            Some("text".to_string())
        );
    }

    #[tokio::test]
    async fn script_shapes_are_keyed_by_visible_name() -> anyhow::Result<()> {
        harnx_core::require_nextest();
        let broker = crate::nats_test_common::spawn_nats_server()
            .await?
            .ok_or_else(|| anyhow::anyhow!("nats-server required"))?;
        let client = async_nats::connect(broker.url()).await?;
        let mut provider = make_test_provider(&client).await;
        let registrations = vec![mapped_registration()];
        (provider.tools, provider.declarations) = build_registered_tools(
            &ServerScope::from_string("provider-tests"),
            None,
            registrations.clone(),
        );
        provider.registrations = registrations;
        let outputs = provider.script_shapes();
        assert_eq!(
            outputs["fs_read"]
                .output
                .as_ref()
                .and_then(|output| output.stdout.as_deref()),
            Some("text")
        );
        assert!(outputs["fs_read"].output_schema.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn a_schema_without_a_mapping_still_has_a_script_shape() -> anyhow::Result<()> {
        harnx_core::require_nextest();
        let broker = crate::nats_test_common::spawn_nats_server()
            .await?
            .ok_or_else(|| anyhow::anyhow!("nats-server required"))?;
        let client = async_nats::connect(broker.url()).await?;
        let mut provider = make_test_provider(&client).await;
        let schema = json!({
            "type": "object",
            "$defs": {"Id": {"type": "string"}},
            "properties": {
                "count": {"type": "integer", "minimum": 0},
                "maybe": {"anyOf": [{"type": "string"}, {"type": "null"}]},
                "id": {"$ref": "#/$defs/Id"},
            }
        });
        let summary = json!({"type": "object", "properties": {
            "count": {"type": "integer"},
            "maybe": {"type": ["string", "null"]},
            "id": {"type": "string"},
        }});
        let spec = |name: &str| ToolSpec {
            cancellation_guarantee: Default::default(),
            name: name.to_string(),
            description: "A tool".to_string(),
            input_schema: json!({ "type": "object" }),
            idempotent_hint: true,
            read_only_hint: true,
            timeout_secs: None,
            meta: None,
        };
        let registrations = vec![global_registration(
            "fs",
            vec![
                spec("counted").with_output_schema(schema),
                spec("plain"),
                spec("empty").with_output_schema(json!({"type": "object"})),
            ],
        )];
        (provider.tools, provider.declarations) = build_registered_tools(
            &ServerScope::from_string("provider-tests"),
            None,
            registrations.clone(),
        );
        provider.registrations = registrations;
        let shapes = provider.script_shapes();
        assert_eq!(shapes.keys().collect::<Vec<_>>(), ["fs_counted"]);
        assert!(shapes["fs_counted"].output.is_none());
        assert_eq!(shapes["fs_counted"].output_schema.as_ref(), Some(&summary));
        Ok(())
    }

    #[tokio::test]
    async fn script_shapes_ignore_raw_name_aliases() -> anyhow::Result<()> {
        harnx_core::require_nextest();
        let broker = crate::nats_test_common::spawn_nats_server()
            .await?
            .ok_or_else(|| anyhow::anyhow!("nats-server required"))?;
        let client = async_nats::connect(broker.url()).await?;
        let mut provider = make_test_provider(&client).await;
        // Both servers expose a tool whose raw name is `read`; only `fs` maps it.
        let registrations = vec![
            read_registration("fs", true),
            read_registration("other", false),
        ];
        (provider.tools, provider.declarations) = build_registered_tools(
            &ServerScope::from_string("provider-tests"),
            None,
            registrations.clone(),
        );
        provider.registrations = registrations;
        let outputs = provider.script_shapes();
        assert_eq!(outputs.keys().collect::<Vec<_>>(), ["fs_read"]);
        assert!(!outputs.contains_key("other_read"));
        assert!(!outputs.contains_key("read"));
        Ok(())
    }

    #[test]
    fn prefixes_registered_tool_with_server_and_keeps_raw_route_name() {
        let scope = ServerScope::from_string("provider-tests");
        harnx_core::require_nextest();
        let registration = global_registration(
            "fs",
            vec![ToolSpec {
                cancellation_guarantee: Default::default(),
                name: "read".to_string(),
                description: "Read a file".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "offset": { "type": ["integer", "null"] }
                    }
                }),
                idempotent_hint: true,
                read_only_hint: true,
                timeout_secs: None,
                meta: None,
            }],
        );

        let (tools, declarations) = build_registered_tools(&scope, None, vec![registration]);

        assert_eq!(declarations[0].name, "fs_read");
        assert_eq!(
            declarations[0].parameters.properties().unwrap()["offset"],
            json!({ "type": ["integer", "null"] })
        );
        assert_eq!(tools["fs_read"].raw_name, "read");
    }

    #[test]
    fn resolves_explicit_zero_as_no_transport_deadline() {
        harnx_core::require_nextest();

        assert_eq!(request_timeout(Some(0)), None);
        assert_eq!(request_timeout(Some(7)), Some(Duration::from_secs(7)));
        assert_eq!(request_timeout(None), Some(DEFAULT_REQUEST_TIMEOUT));
    }

    /// The bridge→NATS policy boundary for a wrapped bash server.
    ///
    /// `harnx-mcp-bridge` copies the wrapped server's advertised
    /// `_meta` timeout into `ToolSpec.timeout_secs`; the bash MCP server
    /// advertises `Some(0)` for spawn, matching the native toolset. Its
    /// nested `wait: {}` can hold the first response for the bash server's
    /// 120-second wait default, longer than this provider's 60-second
    /// backstop, which used to expire the caller's request mid-wait and drop
    /// the execution_id-bearing first response. Registration must resolve that
    /// spec to no elapsed deadline, while a tool that advertises no policy
    /// (any third-party MCP server) keeps the backstop.
    #[test]
    fn bridged_spawn_wait_policy_survives_the_default_request_backstop() {
        let scope = ServerScope::from_string("provider-tests");
        harnx_core::require_nextest();

        const BASH_WAIT_DEFAULT_SECS: u64 = 120;
        let timeout_for = |timeout_secs: Option<u64>| {
            let registration = global_registration(
                "wrapped",
                vec![ToolSpec {
                    cancellation_guarantee: Default::default(),
                    name: "tool".to_string(),
                    description: String::new(),
                    input_schema: json!({ "type": "object" }),
                    idempotent_hint: false,
                    read_only_hint: false,
                    timeout_secs,
                    meta: None,
                }],
            );

            let (tools, _) = build_registered_tools(&scope, None, vec![registration]);
            tools
                .get("wrapped_tool")
                .or_else(|| tools.get("tool"))
                .expect("registered route")
                .request_timeout
        };

        // The 120-second nested-wait default fits inside a disabled deadline
        // where it never fit inside the former 60-second backstop.
        assert!(BASH_WAIT_DEFAULT_SECS > DEFAULT_REQUEST_TIMEOUT.as_secs());
        assert_eq!(timeout_for(Some(0)), None);
        // Tools that advertise nothing keep the caller's default backstop.
        assert_eq!(timeout_for(None), Some(DEFAULT_REQUEST_TIMEOUT));
    }

    /// Native toolsets ship their display templates in `ToolSpec.meta`; if that
    /// stops reaching `ToolDeclaration` the TUI falls back to a YAML dump.
    #[test]
    fn carries_native_toolset_templates_into_tool_declarations() {
        let scope = ServerScope::from_string("provider-tests");
        harnx_core::require_nextest();
        use harnx_toolset::Toolset;

        let specs = harnx_time_tools::TimeToolset::new().tools();
        assert!(!specs.is_empty());
        let registration = global_registration("time", specs.clone());

        let (_, declarations) = build_registered_tools(&scope, None, vec![registration]);

        assert_eq!(declarations.len(), specs.len());
        for declaration in &declarations {
            let template = declaration
                .call_template
                .as_deref()
                .unwrap_or_else(|| panic!("tool '{}' lost its call_template", declaration.name));
            assert!(!template.is_empty());
        }
    }

    /// `ToolSpec.kind()` -> `ToolDeclaration.kind` propagation: native
    /// toolsets declare static kinds (e.g., fs tools have Read/Edit/Search)
    /// and custom specs can also set kind via `with_kind`.
    #[test]
    fn propagates_spec_kind_into_tool_declaration() {
        let scope = ServerScope::from_string("provider-tests");
        harnx_core::require_nextest();
        use harnx_toolset::{ToolProgressKind, Toolset};

        // Native toolset: time-tools have Other kind (simple check)
        let time_specs = harnx_time_tools::TimeToolset::new().tools();
        assert!(!time_specs.is_empty(), "time-tools should have tools");

        let time_registration = global_registration("time", time_specs.clone());

        let (_, time_declarations) = build_registered_tools(&scope, None, vec![time_registration]);

        // Verify time tools have expected kinds (all Other per task spec)
        for decl in &time_declarations {
            assert_eq!(
                decl.kind,
                Some(harnx_core::event::ToolKind::Other),
                "time tool '{}' should have Other kind",
                decl.name
            );
        }

        // Custom spec with explicit kind via with_kind
        let custom_spec = ToolSpec {
            cancellation_guarantee: Default::default(),
            name: "custom_search".to_string(),
            description: "Custom search tool".to_string(),
            input_schema: json!({ "type": "object" }),
            idempotent_hint: false,
            read_only_hint: true,
            timeout_secs: None,
            meta: None,
        }
        .with_kind(ToolProgressKind::Search);

        let custom_registration = global_registration("custom", vec![custom_spec]);

        let (_, custom_declarations) =
            build_registered_tools(&scope, None, vec![custom_registration]);
        assert_eq!(custom_declarations.len(), 1);
        assert_eq!(
            custom_declarations[0].kind,
            Some(harnx_core::event::ToolKind::Search),
            "custom tool with with_kind(Search) should have Search kind"
        );

        // Spec without kind should have None
        let no_kind_spec = ToolSpec {
            cancellation_guarantee: Default::default(),
            name: "no_kind_tool".to_string(),
            description: "Tool without kind".to_string(),
            input_schema: json!({ "type": "object" }),
            idempotent_hint: false,
            read_only_hint: true,
            timeout_secs: None,
            meta: None,
        };

        let no_kind_registration = global_registration("nokind", vec![no_kind_spec]);

        let (_, no_kind_declarations) =
            build_registered_tools(&scope, None, vec![no_kind_registration]);
        assert_eq!(no_kind_declarations.len(), 1);
        assert_eq!(
            no_kind_declarations[0].kind, None,
            "tool without kind declaration should have None kind"
        );
    }

    #[test]
    fn builds_tool_declaration_templates_from_tool_spec_meta() {
        let scope = ServerScope::from_string("provider-tests");
        harnx_core::require_nextest();
        let meta = json!({
            "call_template": "Calling {{tool}}",
            "result_template": "Called {{tool}}"
        })
        .as_object()
        .expect("meta object")
        .clone();
        let registration = global_registration(
            "template-server",
            vec![ToolSpec {
                cancellation_guarantee: Default::default(),
                name: "template_tool".to_string(),
                description: "Tool with display templates".to_string(),
                input_schema: json!({ "type": "object" }),
                idempotent_hint: false,
                read_only_hint: false,
                timeout_secs: None,
                meta: Some(meta),
            }],
        );

        let (_, declarations) = build_registered_tools(&scope, None, vec![registration]);

        assert_eq!(declarations.len(), 1);
        assert_eq!(
            declarations[0].call_template.as_deref(),
            Some("Calling {{tool}}")
        );
        assert_eq!(
            declarations[0].result_template.as_deref(),
            Some("Called {{tool}}")
        );
    }

    /// `open_registry_store` used to map every `get_stream` failure that
    /// `get_key_value` classifies as `KeyValueErrorKind::GetBucket` to
    /// `Ok(None)` — "zero tools registered" — even though `GetBucket` covers
    /// far more than "the stream doesn't exist yet" (see the doc comment on
    /// `open_registry_store`). A broker with JetStream disabled reproduces
    /// exactly that: the stream lookup fails, but not with the JetStream
    /// STREAM_NOT_FOUND code, so it must propagate as an error rather than
    /// silently becoming "no tools".
    #[test]
    fn open_registry_store_propagates_a_non_not_found_stream_error() {
        harnx_core::require_nextest();
        let Some(server) = spawn_nats_server_without_jetstream() else {
            eprintln!(
                "skipping open_registry_store_propagates_a_non_not_found_stream_error: \
                 nats-server binary not found"
            );
            return;
        };
        let runtime = tokio::runtime::Runtime::new().expect("build a tokio runtime");
        let result = runtime.block_on(async {
            let client = async_nats::connect(&server.url)
                .await
                .expect("connect to the JetStream-disabled nats-server");
            let jetstream = async_nats::jetstream::new(client);
            super::open_registry_store(&jetstream).await
        });
        let error = result.expect_err(
            "a stream lookup failure that is not STREAM_NOT_FOUND must propagate, not become \
             Ok(None)",
        );
        assert!(
            !error.to_string().is_empty(),
            "propagated error should carry a message"
        );
    }

    struct JetstreamDisabledNatsServer {
        url: String,
        child: std::process::Child,
    }

    impl Drop for JetstreamDisabledNatsServer {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// Spawns a real `nats-server` with JetStream turned OFF, so a
    /// `get_stream` call fails for a reason other than "not found" without
    /// needing to fake a broker outage or wait out a request timeout.
    fn spawn_nats_server_without_jetstream() -> Option<JetstreamDisabledNatsServer> {
        let binary = std::env::var_os("NATS_SERVER_BIN")
            .map(std::path::PathBuf::from)
            .filter(|path| path.exists())
            .or_else(|| which::which("nats-server").ok())?;
        // `-p -1` lets nats-server take a free port from the kernel and keep it.
        // Allocating one here and dropping the listener first left a window for a
        // concurrently starting server to claim the same port.
        let ports_dir = tempfile::tempdir().expect("create NATS ports dir");
        let mut child = std::process::Command::new(&binary)
            .arg("-a")
            .arg("127.0.0.1")
            .arg("-p")
            .arg("-1")
            .arg("--ports_file_dir")
            .arg(ports_dir.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn nats-server");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let url = loop {
            if let Some(url) = read_first_nats_url(ports_dir.path()) {
                break url;
            }
            if std::time::Instant::now() >= deadline {
                // Reap first: Child does not kill on drop, so panicking here would
                // leave nats-server running for the rest of the test binary.
                let _ = child.kill();
                let _ = child.wait();
                panic!("nats-server did not report its port before the deadline");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        Some(JetstreamDisabledNatsServer { url, child })
    }

    /// nats-server names its ports file `<executable_name>_<pid>.ports`, so scan
    /// the (private) directory instead of rebuilding the name — `NATS_SERVER_BIN`
    /// may point at a differently named binary. A parse failure means the file is
    /// still being written; the caller retries.
    fn read_first_nats_url(dir: &std::path::Path) -> Option<String> {
        for entry in std::fs::read_dir(dir).ok()? {
            let path = entry.ok()?.path();
            if path.extension().is_some_and(|ext| ext == "ports") {
                let contents = std::fs::read_to_string(&path).ok()?;
                let parsed: serde_json::Value = serde_json::from_str(&contents).ok()?;
                return Some(parsed.get("nats")?.get(0)?.as_str()?.to_string());
            }
        }
        None
    }

    #[derive(Default)]
    struct RecordingProgress {
        updates: std::sync::Mutex<Vec<harnx_core::tool::ToolUpdatePatch>>,
        received: tokio::sync::Notify,
    }

    impl RecordingProgress {
        async fn wait_for_updates(&self, count: usize) {
            loop {
                if self.updates.lock().unwrap().len() >= count {
                    return;
                }
                self.received.notified().await;
            }
        }
    }

    impl harnx_core::tool::ToolProgress for RecordingProgress {
        fn update(&self, patch: harnx_core::tool::ToolUpdatePatch) {
            self.updates.lock().unwrap().push(patch);
            self.received.notify_one();
        }
    }

    fn progress_titles(progress: &RecordingProgress) -> Vec<String> {
        progress
            .updates
            .lock()
            .unwrap()
            .iter()
            .filter_map(|patch| patch.title.clone())
            .collect()
    }

    fn unwrap_tool_result(
        result: Result<harnx_core::tool::ToolProviderOutput, harnx_core::tool::ToolError>,
    ) -> harnx_core::tool::ToolProviderOutput {
        match result {
            Ok(output) => output,
            Err(harnx_core::tool::ToolError::Recoverable(error))
            | Err(harnx_core::tool::ToolError::Fatal(error)) => {
                panic!("tool call failed: {error:#}")
            }
        }
    }

    async fn progress_test_provider(
        client: async_nats::Client,
        instance_id: ServerScope,
    ) -> NatsToolProvider {
        let subscription = client
            .subscribe(instance_id.control_subject())
            .await
            .expect("subscribe to progress control subject");
        client.flush().await.expect("flush progress subscription");
        NatsToolProvider {
            client,
            instance_id,
            parent_session_id: None,
            run_context: None,
            parent_agent: None,
            parent_local_session_id: None,
            session_owner: None,
            tools: HashMap::from([(
                "stream".to_string(),
                RegisteredTool {
                    server: "progress-test".to_string(),
                    raw_name: "stream".to_string(),
                    request_timeout: Some(DEFAULT_REQUEST_TIMEOUT),
                    ..Default::default()
                },
            )]),
            registrations: Vec::new(),
            active_package: None,
            declarations: Vec::new(),
            registry: None,
            journal_replicas: 1,
            journal: Default::default(),
            progress_dispatcher: ProgressDispatcher::new(subscription),
            in_flight: NatsInFlightCalls::default(),
        }
    }

    async fn publish_progress(
        client: &async_nats::Client,
        subject: &str,
        call_id: &str,
        title: String,
    ) {
        use harnx_toolset::{ProgressChunk, ProgressMessage, ToolProgressPatch};
        let message = ProgressMessage {
            call_id: call_id.to_string(),
            chunk: ProgressChunk::V1(ToolProgressPatch {
                title: Some(title),
                ..Default::default()
            }),
        };
        client
            .publish(
                subject.to_string(),
                serde_json::to_vec(&message).unwrap().into(),
            )
            .await
            .unwrap();
        client.flush().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    // qual:allow(complexity, max_function_lines=185) reason: "One end-to-end wiring test walks a call through dispatch, progress coalescing, reply racing and cleanup; splitting it would leave the ordering race untested."
    async fn progress_routes_parallel_calls_reconciles_fast_reply_and_cleans_up_cancel() {
        use futures_util::StreamExt;
        use harnx_core::abort::create_abort_signal;
        use harnx_core::tool::ToolProvider;
        use harnx_toolset::{ToolReply, ToolRequest, CAPABILITY_TOOL_PROGRESS};
        use std::sync::Arc;

        harnx_core::require_nextest();
        let Some(server) = spawn_nats_server_without_jetstream() else {
            eprintln!("skipping progress routing test: nats-server missing");
            return;
        };
        let client = async_nats::connect(&server.url)
            .await
            .expect("connect to test NATS server");
        let instance_id = ServerScope::new();
        let subject = instance_id.tool_subject("progress-test", "stream");
        let control_subject = instance_id.control_subject();
        let mut requests = client
            .subscribe(subject)
            .await
            .expect("subscribe to progress test tool");
        client.flush().await.expect("flush tool subscription");
        let alpha = Arc::new(RecordingProgress::default());
        let beta = Arc::new(RecordingProgress::default());
        let responder_alpha = alpha.clone();
        let responder_beta = beta.clone();
        let responder_client = client.clone();
        let responder = tokio::spawn(async move {
            while let Some(message) = requests.next().await {
                let client = responder_client.clone();
                let control_subject = control_subject.clone();
                let alpha_progress = responder_alpha.clone();
                let beta_progress = responder_beta.clone();
                tokio::spawn(async move {
                    let request: ToolRequest =
                        serde_json::from_slice(&message.payload).expect("decode tool request");
                    let label = request.args["label"].as_str().unwrap().to_string();
                    let mode = request.args["mode"].as_str().unwrap_or("normal");
                    let supports_progress = request.capabilities.contains(CAPABILITY_TOOL_PROGRESS);
                    assert_eq!(supports_progress, label != "legacy");
                    if mode != "final_only" {
                        publish_progress(
                            &client,
                            &control_subject,
                            &request.call_id,
                            format!("{label}-live"),
                        )
                        .await;
                    }
                    if mode == "cancel" {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        publish_progress(
                            &client,
                            &control_subject,
                            &request.call_id,
                            format!("{label}-late"),
                        )
                        .await;
                        return;
                    }
                    match label.as_str() {
                        "alpha" => alpha_progress.wait_for_updates(1).await,
                        "beta" => beta_progress.wait_for_updates(1).await,
                        _ => {}
                    }
                    let final_progress =
                        supports_progress.then(|| harnx_toolset::ToolProgressPatch {
                            title: Some(format!("{label}-final")),
                            ..Default::default()
                        });
                    let reply = ToolReply {
                        input_required: None,
                        call_id: request.call_id,
                        result: Ok(json!({"label": label})),
                        final_progress,
                    };
                    client
                        .publish(
                            message.reply.expect("request has reply subject"),
                            serde_json::to_vec(&reply).unwrap().into(),
                        )
                        .await
                        .unwrap();
                });
            }
        });
        let provider = progress_test_provider(client, instance_id).await;

        let alpha_abort = create_abort_signal();
        let beta_abort = create_abort_signal();
        let (alpha_result, beta_result) = tokio::join!(
            provider.call_tool_with_progress(
                "stream",
                json!({"label": "alpha"}),
                transcript_call("logical-alpha"),
                &alpha_abort,
                alpha.clone(),
            ),
            provider.call_tool_with_progress(
                "stream",
                json!({"label": "beta"}),
                transcript_call("logical-beta"),
                &beta_abort,
                beta.clone(),
            )
        );
        assert_eq!(unwrap_tool_result(alpha_result)["label"], "alpha");
        assert_eq!(unwrap_tool_result(beta_result)["label"], "beta");
        assert_eq!(progress_titles(&alpha), ["alpha-live", "alpha-final"]);
        assert_eq!(progress_titles(&beta), ["beta-live", "beta-final"]);

        let fast = Arc::new(RecordingProgress::default());
        let fast_abort = create_abort_signal();
        let fast_result = unwrap_tool_result(
            provider
                .call_tool_with_progress(
                    "stream",
                    json!({"label": "fast", "mode": "final_only"}),
                    transcript_call("logical-fast"),
                    &fast_abort,
                    fast.clone(),
                )
                .await,
        );
        assert_eq!(fast_result["label"], "fast");
        assert_eq!(progress_titles(&fast), ["fast-final"]);

        let cancel = Arc::new(RecordingProgress::default());
        let abort = create_abort_signal();
        let abort_trigger = abort.clone();
        let cancel_progress = cancel.clone();
        let abort_task = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_secs(2), cancel_progress.wait_for_updates(1))
                .await
                .expect("cancel progress update must arrive before abort");
            abort_trigger.set_ctrlc();
        });
        let cancelled = tokio::time::timeout(
            Duration::from_secs(2),
            provider.call_tool_with_progress(
                "stream",
                json!({"label": "cancel", "mode": "cancel"}),
                transcript_call("logical-cancel"),
                &abort,
                cancel.clone(),
            ),
        )
        .await
        .expect("cancelled progress call must not hang");
        assert!(cancelled.is_err());
        abort_task.await.expect("abort trigger task succeeds");
        tokio::time::sleep(Duration::from_millis(450)).await;
        assert_eq!(progress_titles(&cancel), ["cancel-live"]);

        let after_cancel = Arc::new(RecordingProgress::default());
        let after_abort = create_abort_signal();
        let result = unwrap_tool_result(
            provider
                .call_tool_with_progress(
                    "stream",
                    json!({"label": "after", "mode": "final_only"}),
                    transcript_call("logical-after"),
                    &after_abort,
                    after_cancel.clone(),
                )
                .await,
        );

        let legacy_abort = create_abort_signal();
        let legacy = unwrap_tool_result(
            provider
                .call_tool(
                    "stream",
                    json!({"label": "legacy", "mode": "final_only"}),
                    &legacy_abort,
                )
                .await,
        );
        assert_eq!(legacy["label"], "legacy");
        assert_eq!(result["label"], "after");
        assert_eq!(progress_titles(&after_cancel), ["after-final"]);

        responder.abort();
    }
}

#[cfg(test)]
mod attachment_encoding_tests;
