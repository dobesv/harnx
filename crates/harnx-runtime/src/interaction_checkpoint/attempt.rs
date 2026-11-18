use super::{
    records::{call_prefix, AttemptStart, DispatchCompleted, DispatchIntent, DispatchKind},
    snapshot::ResultSnapshot,
    store::CheckpointStore,
};
use crate::nats_lease::NatsSessionLease;
use anyhow::{ensure, Result};
use harnx_core::{interaction::InteractionStep, tool::ToolCall};
use harnx_engine::tool::MaterializedToolResult;
use std::{future::Future, sync::Arc};

#[derive(Clone)]
pub(crate) struct Attempt {
    store: CheckpointStore,
    lease: Arc<NatsSessionLease>,
    session_storage_key: String,
    prefix: String,
    id: String,
}

tokio::task_local! { static ATTEMPT: Attempt; }

pub(crate) fn current() -> Option<Attempt> {
    ATTEMPT.try_with(Clone::clone).ok()
}
pub(crate) async fn scope<F: Future>(attempt: Attempt, future: F) -> F::Output {
    ATTEMPT.scope(attempt, future).await
}

tokio::task_local! { static STEP_PATH: Vec<InteractionStep>; }
pub(crate) fn step_path() -> Vec<InteractionStep> {
    STEP_PATH.try_with(Clone::clone).unwrap_or_default()
}
pub(crate) async fn with_step_path<F: Future>(path: Vec<InteractionStep>, future: F) -> F::Output {
    STEP_PATH.scope(path, future).await
}

impl Attempt {
    pub async fn start(
        store: CheckpointStore,
        lease: Arc<NatsSessionLease>,
        record: AttemptStart,
    ) -> Result<Self> {
        ensure!(
            lease.revalidate_ownership().await?,
            "session lease lost before middleware attempt"
        );
        let prefix = call_prefix(
            &record.session_storage_key,
            record.tool_round_seq,
            &record.tool_call_id,
        );
        crate::fault_injection::hit(
            &record.session_storage_key,
            crate::fault_injection::RecoveryFaultPoint::AttemptStartBefore,
        )
        .await;
        store
            .put(
                &prefix,
                &format!("attempts.{}.start", record.attempt_id),
                &record,
            )
            .await?;
        crate::fault_injection::hit(
            &record.session_storage_key,
            crate::fault_injection::RecoveryFaultPoint::AttemptStartAfter,
        )
        .await;
        Ok(Self {
            store,
            lease,
            session_storage_key: record.session_storage_key.clone(),
            prefix,
            id: record.attempt_id,
        })
    }

    pub async fn dispatch(
        &self,
        kind: DispatchKind,
        path: Vec<InteractionStep>,
        call: ToolCall,
    ) -> Result<String> {
        ensure!(
            self.lease.revalidate_ownership().await?,
            "session lease lost before middleware dispatch intent"
        );
        let id = uuid::Uuid::new_v4().to_string();
        let record = DispatchIntent {
            version: 1,
            attempt_id: self.id.clone(),
            physical_invocation_id: id.clone(),
            kind,
            step_path: path,
            call,
            fence_token: self.lease.fence_token(),
        };
        crate::fault_injection::hit(
            &self.session_storage_key,
            crate::fault_injection::RecoveryFaultPoint::DispatchIntentBefore,
        )
        .await;
        self.store
            .put(
                &self.prefix,
                &format!("attempts.{}.dispatches.{id}.intent", self.id),
                &record,
            )
            .await?;
        crate::fault_injection::hit(
            &self.session_storage_key,
            crate::fault_injection::RecoveryFaultPoint::DispatchIntentAfter,
        )
        .await;
        // Intent commits before the ownership recheck and physical invocation.
        ensure!(
            self.lease.revalidate_ownership().await?,
            "session lease lost after middleware dispatch intent"
        );
        Ok(id)
    }

    pub async fn completed(&self, id: &str, result: &MaterializedToolResult) -> Result<()> {
        ensure!(
            self.lease.revalidate_ownership().await?,
            "session lease lost before downstream checkpoint"
        );
        crate::fault_injection::hit(
            &self.session_storage_key,
            crate::fault_injection::RecoveryFaultPoint::DownstreamSnapshotBefore,
        )
        .await;
        let snapshot = self
            .store
            .put(
                &self.prefix,
                &format!("attempts.{}.dispatches.{id}.snapshot", self.id),
                &ResultSnapshot::new(result),
            )
            .await?;
        crate::fault_injection::hit(
            &self.session_storage_key,
            crate::fault_injection::RecoveryFaultPoint::DownstreamSnapshotAfter,
        )
        .await;
        let completed = DispatchCompleted {
            version: 1,
            physical_invocation_id: id.into(),
            snapshot,
            fence_token: self.lease.fence_token(),
        };
        crate::fault_injection::hit(
            &self.session_storage_key,
            crate::fault_injection::RecoveryFaultPoint::DownstreamCompletionBefore,
        )
        .await;
        self.store
            .put(
                &self.prefix,
                &format!("attempts.{}.dispatches.{id}.completed", self.id),
                &completed,
            )
            .await?;
        crate::fault_injection::hit(
            &self.session_storage_key,
            crate::fault_injection::RecoveryFaultPoint::DownstreamCompletionAfter,
        )
        .await;
        Ok(())
    }
}
