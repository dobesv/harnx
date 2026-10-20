//! Lazy worker handoff preparation. Reservation precedes durable ToolResults.
//!
//! The handoff preparation callback runs before the tool result is persisted.
//! Worker uses invocation-keyed reservation for retry stability. Caller-supplied
//! IDs pass through unchanged. The callback does not need Requested/Reserved
//! provenance because worker dispatch does not run caller-ownership validation.
use super::super::SessionActivationRoute;
use crate::config::GlobalConfig;
use crate::nats_session_metadata::SessionMetadataStore;
use crate::tool::{PrepareHandoffFn, PreparedHandoff};
use crate::utils::AbortSignal;
use anyhow::{Context, Result};
use async_nats::jetstream;
use harnx_core::session::SessionLogEntry;
use std::sync::Arc;

#[derive(Clone)]
pub(super) struct NatsHandoffContext {
    pub config: GlobalConfig,
    pub source_session_id: String,
    pub cluster_key: String,
    pub jetstream_ctx: jetstream::Context,
    pub activation_route: SessionActivationRoute,
    pub replicas: usize,
}

pub(super) struct HandoffTarget {
    pub destination: HandoffDestination,
    pub initializer: crate::SessionInitializer,
    pub parent: crate::nats_session_metadata::RunLimitsRecord,
    pub invocation_id: String,
    pub admitted_at: chrono::DateTime<chrono::Utc>,
}

impl HandoffTarget {
    pub async fn resolve_session_id(&self, requested_session_id: Option<String>) -> Result<String> {
        if let Some(id) = requested_session_id.filter(|id| !id.trim().is_empty()) {
            return Ok(id);
        }
        let store =
            SessionMetadataStore::ensure(&self.destination.jetstream, self.destination.replicas)
                .await?;
        // Both synthetic tools and provider results need retry-stable reservation.
        crate::utils::session_name::reserve_invocation_session_id(
            &store,
            &self.initializer,
            &self.invocation_id,
            self.admitted_at.timestamp_millis().try_into()?,
        )
        .await
    }
}

impl NatsHandoffContext {
    pub fn callback(self, abort_signal: AbortSignal) -> Arc<PrepareHandoffFn> {
        Arc::new(move |agent, requested_session_id, tool_call_id| {
            let context = self.clone();
            let abort_signal = abort_signal.clone();
            Box::pin(async move {
                let target = context.resolve_target(&agent, &tool_call_id).await?;
                anyhow::ensure!(!abort_signal.aborted(), "interrupted at handoff-create");
                if target.parent.is_expired_at(chrono::Utc::now()) {
                    return Err(crate::nats_session_metadata::run_limits::DeadlineExpired.into());
                }
                let session_id = target.resolve_session_id(requested_session_id).await?;
                Ok(PreparedHandoff {
                    agent: target.destination.committed_agent,
                    session_id,
                })
            })
        })
    }

    pub async fn resolve_target(
        &self,
        agent: &str,
        handoff_tool_call_id: &str,
    ) -> Result<HandoffTarget> {
        let parent = self
            .config
            .read()
            .run_context
            .clone()
            .context("handoff requires inherited run context")?;

        let round = find_tool_round(
            self.jetstream_ctx.clone(),
            &self.source_session_id,
            handoff_tool_call_id,
        )
        .await?;

        let edge = format!("{round}:{handoff_tool_call_id}");
        let invocation_id =
            harnx_core::session_identity::session_key(Some(parent.invocation_id.as_str()), &edge);

        let source_store = crate::nats_session_metadata::SessionMetadataStore::ensure(
            &self.jetstream_ctx,
            self.replicas,
        )
        .await?;

        let lineage = admission_cas_lineage(
            &source_store,
            &self.source_session_id,
            &invocation_id,
            &parent,
        )
        .await?;

        let parent: crate::nats_session_metadata::RunLimitsRecord =
            serde_json::from_value(lineage.snapshot)?;
        let admitted_at =
            chrono::DateTime::from_timestamp_millis(lineage.started_at_ms.try_into()?)
                .context("invalid handoff admission time")?;

        let destination = resolve_handoff_destination(self, agent).await?;
        let initializer = handoff_initializer(self, &source_store, &destination).await?;

        Ok(HandoffTarget {
            destination,
            initializer,
            parent,
            invocation_id,
            admitted_at,
        })
    }
}

async fn handoff_initializer(
    args: &NatsHandoffContext,
    source_store: &crate::nats_session_metadata::SessionMetadataStore,
    destination: &HandoffDestination,
) -> Result<crate::SessionInitializer> {
    let source_metadata = source_store
        .get(&args.source_session_id)
        .await?
        .context("handoff source metadata missing")?;
    let inherited_identity =
        crate::nats_session_metadata::session_properties(&source_metadata.metadata)?
            .inherited_user_id();
    Ok(
        crate::SessionInitializer::named(destination.agent.clone(), Default::default())
            .with_properties(inherited_identity)
            .with_default_user_id(
                args.config
                    .read()
                    .default_user_id_for_cluster(&destination.cluster),
            ),
    )
}

/// Finds the sequence number of the durable tool round containing the handoff call.
async fn find_tool_round(
    source_js: async_nats::jetstream::Context,
    source_session_id: &str,
    handoff_id: &str,
) -> Result<u64> {
    let entries = crate::nats_session_log::NatsSessionLog::new(source_js, source_session_id)
        .load_events_latest_async()
        .await?;
    entries
        .iter()
        .rev()
        .find_map(|(seq, entry)| match entry {
            SessionLogEntry::ToolCalls { calls, .. }
                if calls
                    .iter()
                    .any(|call| call.id.as_deref() == Some(handoff_id)) =>
            {
                Some(*seq)
            }
            _ => None,
        })
        .context("handoff has no durable tool round")
}

/// Records handoff admission via CAS (compare-and-swap) on the metadata store.
/// Returns the confirmed lineage (either our proposed value or a concurrent update).
async fn admission_cas_lineage(
    source_store: &crate::nats_session_metadata::SessionMetadataStore,
    source_session_id: &str,
    invocation_id: &str,
    parent: &crate::nats_session_metadata::RunLimitsRecord,
) -> Result<harnx_toolset::AutonomousRunContext> {
    let key = format!(
        "sessions/{}/handoff_admissions/{invocation_id}",
        source_session_id
    );
    let proposed = harnx_toolset::AutonomousRunContext {
        snapshot: serde_json::to_value(parent)?,
        started_at_ms: chrono::Utc::now().timestamp_millis().try_into()?,
    };
    Ok(match source_store.leader_value(&key).await? {
        Some(bytes) => serde_json::from_slice::<harnx_toolset::AutonomousRunContext>(&bytes)?,
        None => {
            if source_store
                .kv_store()
                .update(&key, serde_json::to_vec(&proposed)?.into(), 0)
                .await
                .is_err()
            {
                serde_json::from_slice(
                    &source_store
                        .leader_value(&key)
                        .await?
                        .context("handoff admission unconfirmed")?,
                )?
            } else {
                proposed
            }
        }
    })
}

pub(super) struct HandoffDestination {
    agent: String,
    pub(super) cluster: String,
    pub(super) activation_route: SessionActivationRoute,
    pub(super) committed_agent: String,
    pub(super) replicas: usize,
    pub(super) client: async_nats::Client,
    pub(super) jetstream: jetstream::Context,
}

async fn resolve_handoff_destination(
    args: &NatsHandoffContext,
    agent: &str,
) -> Result<HandoffDestination> {
    use harnx_core::agent_ref::AgentRef;

    let target = match AgentRef::parse(agent) {
        // Preparation qualifies shared-cluster targets for the result marker.
        // Dispatch must still reuse the source connection and replica settings.
        AgentRef::Remote { agent, cluster }
            if cluster == args.cluster_key
                && matches!(args.activation_route, SessionActivationRoute::ClusterShared) =>
        {
            AgentRef::Local(agent)
        }
        target => target,
    };
    match target {
        AgentRef::Local(target_agent) => {
            let target_agent = target_agent.into_owned();
            let committed_agent = match &args.activation_route {
                SessionActivationRoute::ClusterShared => {
                    format!("{target_agent}@{}", args.cluster_key)
                }
                SessionActivationRoute::WorkerTargeted { .. } => target_agent.clone(),
            };
            Ok(HandoffDestination {
                agent: target_agent,
                cluster: args.cluster_key.clone(),
                activation_route: args.activation_route.clone(),
                committed_agent,
                replicas: args.replicas,
                client: args.jetstream_ctx.client().clone(),
                jetstream: args.jetstream_ctx.clone(),
            })
        }
        AgentRef::Remote {
            agent: target_agent,
            cluster,
        } => {
            let target_cluster = cluster.into_owned();
            let config = args.config.read().clone();
            let replicas = config
                .resolve_nats_server(&target_cluster)
                .await?
                .resolved_replicas();
            let client = config
                .nats_client(&target_cluster)
                .await
                .with_context(|| format!("connect to handoff target cluster '{target_cluster}'"))?;
            Ok(HandoffDestination {
                agent: target_agent.into_owned(),
                cluster: target_cluster,
                activation_route: SessionActivationRoute::ClusterShared,
                committed_agent: agent.to_string(),
                replicas,
                jetstream: jetstream::new(client.clone()),
                client,
            })
        }
    }
}
