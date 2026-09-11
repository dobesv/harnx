use crate::*;
use anyhow::{bail, ensure, Context, Result};
use async_nats::jetstream::{self, kv, stream};
use futures_util::StreamExt;
use std::collections::BTreeSet;

#[derive(Clone, Debug)]
pub struct ExecutionStore {
    kv: kv::Store,
}

impl ExecutionStore {
    pub async fn watch(&self) -> Result<kv::Watch> {
        Ok(self.kv.watch_all().await?)
    }
    pub async fn ensure(js: &jetstream::Context, replicas: usize) -> Result<Self> {
        let kv = match js
            .create_key_value(kv::Config {
                bucket: BUCKET.into(),
                history: 1,
                num_replicas: replicas,
                storage: stream::StorageType::File,
                ..Default::default()
            })
            .await
        {
            Ok(kv) => kv,
            Err(_) => {
                harnx_nats_common::registry::reconcile_bucket_replicas(js, BUCKET, replicas)
                    .await?;
                js.get_key_value(BUCKET).await?
            }
        };
        Ok(Self { kv })
    }

    pub async fn get(&self, reference: &OperationRef) -> Result<Option<Operation>> {
        reference.validate()?;
        Ok(self
            .entry(&reference.key())
            .await?
            .map(|(operation, _)| operation))
    }

    async fn entry(&self, key: &str) -> Result<Option<(Operation, u64)>> {
        let Some(entry) = self.kv.entry(key).await? else {
            return Ok(None);
        };
        if entry.operation != kv::Operation::Put {
            return Ok(None);
        }
        Ok(Some((
            serde_json::from_slice(&entry.value)?,
            entry.revision,
        )))
    }

    pub async fn current(&self, session: &str) -> Result<Option<Operation>> {
        let Some(value) = self.kv.get(current_key(session)).await? else {
            return Ok(None);
        };
        let reference: OperationRef = serde_json::from_slice(&value)?;
        Ok(Some(self.get(&reference).await?.context(
            "current execution record missing; cancellation unconfirmed",
        )?))
    }

    /// Install a generation only after the previous generation is terminal.
    /// A losing candidate never executes; all admissions CAS the winning record.
    pub async fn session(
        &self,
        session: &str,
        parent: Option<OperationRef>,
        execution_id: Option<&str>,
    ) -> Result<Operation> {
        loop {
            let key = current_key(session);
            let pointer = self.kv.entry(&key).await?;
            let revision = pointer.as_ref().map_or(0, |e| e.revision);
            let previous = pointer
                .as_ref()
                .filter(|entry| entry.operation == kv::Operation::Put)
                .map(|entry| serde_json::from_slice::<OperationRef>(&entry.value))
                .transpose()?;
            if let Some(entry) = pointer.filter(|e| e.operation == kv::Operation::Put) {
                let reference = serde_json::from_slice(&entry.value)?;
                let current = self
                    .get(&reference)
                    .await?
                    .context("current execution missing")?;
                if !current.state.is_terminal() {
                    ensure!(
                        current.state.accepts_work(),
                        "session cancellation is {:?}; retry cancellation before prompting",
                        current.state
                    );
                    if parent.is_some() {
                        ensure!(
                            current.parent == parent,
                            "session already belongs to another active invocation"
                        );
                    }
                    ensure!(
                        !current.sealed,
                        "session owner is finishing; retry admission"
                    );
                    self.check_ancestors(&current.reference).await?;
                    return Ok(current);
                }
            }
            let id = execution_id
                .map(str::to_string)
                .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
            let candidate = Operation::preparing(
                OperationRef::new(session, id),
                OperationKind::Session,
                parent.clone(),
            );
            self.create(&candidate).await?;
            let payload = serde_json::to_vec(&candidate.reference)?;
            match self.kv.update(&key, payload.into(), revision).await {
                Ok(_) => {
                    if let Some(previous) = &previous {
                        self.purge_observed_terminal(previous).await?;
                    }
                    if let Some(parent) = &parent {
                        self.register(&candidate.reference, parent).await?;
                    }
                    return self
                        .get(&candidate.reference)
                        .await?
                        .context("new execution missing");
                }
                Err(error) if error.kind() == kv::UpdateErrorKind::WrongLastRevision => {
                    self.cancel_unstarted(&candidate.reference).await?;
                    self.kv.purge(candidate.reference.key()).await?;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    pub async fn create(&self, operation: &Operation) -> Result<()> {
        operation.reference.validate()?;
        self.kv
            .create(
                operation.reference.key(),
                serde_json::to_vec(operation)?.into(),
            )
            .await?;
        Ok(())
    }

    /// Two-phase registration: no work starts until the parent CAS wins and
    /// the entire ancestor chain has been checked again.
    pub async fn child(&self, reference: OperationRef, parent: OperationRef) -> Result<Operation> {
        let child = Operation::preparing(reference, OperationKind::Tool, Some(parent.clone()));
        self.create(&child).await?;
        self.register(&child.reference, &parent).await?;
        Ok(child)
    }

    async fn register(&self, child: &OperationRef, parent: &OperationRef) -> Result<()> {
        let result = self
            .mutate(parent, |operation| {
                ensure!(
                    operation.accepts_work(),
                    "parent is cancelling; child cannot start"
                );
                operation.children.insert(child.clone());
                Ok(())
            })
            .await;
        if result.is_err() {
            self.cancel_unstarted(child).await?;
        }
        result?;
        if let Err(error) = self.check_ancestors(child).await {
            self.cancel_unstarted(child).await?;
            return Err(error);
        }
        Ok(())
    }

    async fn cancel_unstarted(&self, reference: &OperationRef) -> Result<()> {
        self.mutate(reference, |operation| {
            ensure!(
                operation.owner.is_none(),
                "cannot cancel active owner without cleanup"
            );
            operation.request_cancel(&uuid::Uuid::now_v7().to_string(), false)?;
            operation.transition(OperationState::Quiescing)?;
            operation.owner_stopped = true;
            operation.cancel_recorded = true;
            operation.transition(OperationState::Cancelled)
        })
        .await?;
        Ok(())
    }

    pub async fn mutate(
        &self,
        reference: &OperationRef,
        update: impl Fn(&mut Operation) -> Result<()>,
    ) -> Result<Operation> {
        reference.validate()?;
        loop {
            let (mut operation, revision) = self
                .entry(&reference.key())
                .await?
                .context("execution missing; cancellation unconfirmed")?;
            let before = serde_json::to_vec(&operation)?;
            let previous_state = operation.state;
            update(&mut operation)?;
            if serde_json::to_vec(&operation)? == before {
                return Ok(operation);
            }
            match self
                .kv
                .update(
                    reference.key(),
                    serde_json::to_vec(&operation)?.into(),
                    revision,
                )
                .await
            {
                Ok(_) => {
                    crate::telemetry::transition(previous_state, &operation);
                    return Ok(operation);
                }
                Err(error) if error.kind() == kv::UpdateErrorKind::WrongLastRevision => {
                    tokio::task::yield_now().await
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    pub async fn claim(&self, reference: &OperationRef, owner: Owner) -> Result<Operation> {
        self.mutate(reference, |operation| {
            ensure!(!operation.state.is_terminal(), "execution already terminal");
            if let Some(previous) = &operation.owner {
                ensure!(
                    previous == &owner
                        || (operation.kind == OperationKind::Session
                            && owner.fence > previous.fence),
                    "stale execution owner"
                );
            }
            operation.owner = Some(owner.clone());
            operation.owner_stopped = false;
            operation.sealed = false;
            if operation.state == OperationState::Preparing {
                operation.transition(OperationState::Running)?;
            }
            Ok(())
        })
        .await
    }

    pub async fn reserve_prompt(&self, reference: &OperationRef, message_id: &str) -> Result<()> {
        self.mutate(reference, |operation| {
            ensure!(
                operation.accepts_work(),
                "session is cancelling; prompt rejected"
            );
            operation
                .admissions
                .entry(message_id.into())
                .or_insert(None);
            Ok(())
        })
        .await?;
        Ok(())
    }

    pub async fn commit_prompt(
        &self,
        reference: &OperationRef,
        message_id: &str,
        seq: u64,
    ) -> Result<()> {
        self.mutate(reference, |operation| {
            let reserved = operation
                .admissions
                .get_mut(message_id)
                .context("prompt was not admitted")?;
            ensure!(
                reserved.is_none_or(|previous| previous == seq),
                "prompt reservation sequence changed"
            );
            *reserved = Some(seq);
            Ok(())
        })
        .await?;
        Ok(())
    }

    pub async fn request_cancel(
        &self,
        session: &str,
        request: CancelRequest,
    ) -> Result<CancelReceipt> {
        let Some(current) = self.current(session).await? else {
            return Ok(CancelReceipt::idle());
        };
        if request
            .expected_execution_id
            .as_ref()
            .is_some_and(|id| id != &current.reference.execution_id)
        {
            return Ok(CancelReceipt::idle());
        }
        let already = current.state.cancelling();
        let operation = self
            .cancel_operation(&current.reference, None, request.retry)
            .await?;
        Ok(CancelReceipt::from_operation(&operation, already))
    }

    pub async fn cancel_operation(
        &self,
        reference: &OperationRef,
        cancellation_id: Option<&str>,
        retry: bool,
    ) -> Result<Operation> {
        let id = cancellation_id
            .map(str::to_string)
            .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
        self.mutate(reference, |operation| operation.request_cancel(&id, retry))
            .await
    }

    pub async fn quiesce(&self, reference: &OperationRef, owner: &Owner) -> Result<Operation> {
        self.mutate(reference, |operation| {
            operation.check_owner(owner)?;
            if operation.state == OperationState::CancelRequested {
                operation.transition(OperationState::Quiescing)?;
            }
            Ok(())
        })
        .await
    }

    /// Watch subscription is installed before the preflight read. Descendants
    /// observe ancestors themselves, so a dead intermediate owner cannot lose
    /// the cancellation cascade. A broker/graph failure is fail-closed.
    pub async fn watch_cancellation(&self, reference: &OperationRef) -> Result<()> {
        let mut watch = self.kv.watch_all().await?;
        self.check_ancestors(reference).await?;
        while let Some(entry) = watch.next().await {
            entry?;
            self.check_ancestors(reference).await?;
        }
        bail!("execution control watch closed")
    }

    pub async fn check_ancestors(&self, reference: &OperationRef) -> Result<()> {
        let mut next = Some(reference.clone());
        let mut seen = BTreeSet::new();
        while let Some(reference) = next {
            ensure!(
                seen.insert(reference.clone()),
                "execution graph cycle; cancellation unconfirmed"
            );
            let operation = self
                .get(&reference)
                .await?
                .context("execution ancestor missing; cancellation unconfirmed")?;
            ensure!(
                operation.state.accepts_work(),
                "execution {} is {:?}",
                reference.execution_id,
                operation.state
            );
            if let Some(parent) = &operation.parent {
                let parent = self
                    .get(parent)
                    .await?
                    .context("execution ancestor missing; cancellation unconfirmed")?;
                ensure!(
                    parent.children.contains(&operation.reference),
                    "child registration incomplete; refusing detached execution"
                );
            }
            next = operation.parent;
        }
        Ok(())
    }

    /// Called after cleanup/lease release. Children still running keep the
    /// operation nonterminal; they may later converge without another request.
    pub async fn owner_stopped(
        &self,
        reference: &OperationRef,
        owner: &Owner,
    ) -> Result<Operation> {
        let operation = self
            .mutate(reference, |operation| {
                operation.check_owner(owner)?;
                operation.owner_stopped = true;
                operation.reconcile_completion()
            })
            .await?;
        if operation.state.is_terminal() {
            return Ok(operation);
        }
        self.status(reference).await
    }

    pub async fn status(&self, reference: &OperationRef) -> Result<Operation> {
        let mut stack = vec![(reference.clone(), None, false)];
        let mut seen = BTreeSet::new();
        let mut root = None;
        while let Some((current, parent, visited)) = stack.pop() {
            let Some(operation) = self.get(&current).await? else {
                // Another observer may already have removed a terminal child.
                // Only an edge that still exists represents a missing blocker.
                if let Some(parent) = &parent {
                    if self
                        .get(parent)
                        .await?
                        .is_some_and(|op| !op.children.contains(&current))
                    {
                        continue;
                    }
                }
                bail!("execution descendant missing; cancellation unconfirmed");
            };
            if visited {
                let operation = self.reconcile_one(&current).await?;
                if current == *reference {
                    root = Some(operation);
                }
                continue;
            }
            ensure!(
                seen.insert(current.clone()),
                "execution graph cycle; cancellation unconfirmed"
            );
            stack.push((current.clone(), parent, true));
            for child in operation.children {
                stack.push((child, Some(current.clone()), false));
            }
        }
        root.context("execution missing")
    }

    async fn reconcile_one(&self, reference: &OperationRef) -> Result<Operation> {
        let operation = self.get(reference).await?.context("execution missing")?;
        if operation.state.is_terminal() {
            return Ok(operation);
        }
        let mut finished = BTreeSet::new();
        for child in &operation.children {
            if self
                .get(child)
                .await?
                .is_some_and(|child| child.state.is_terminal())
            {
                finished.insert(child.clone());
            }
        }
        let result = self
            .mutate(reference, |operation| {
                if operation.state.is_terminal() {
                    return Ok(());
                }
                let previous_children = operation.children.len();
                operation.children.retain(|child| !finished.contains(child));
                if previous_children != operation.children.len() {
                    if let Some(cancel) = operation.cancellation.as_mut() {
                        cancel.progress_at = chrono::Utc::now();
                    }
                }
                operation.reconcile_completion()?;
                operation.expire_progress()?;
                Ok(())
            })
            .await?;
        for child in finished {
            self.purge_observed_terminal(&child).await?;
        }
        Ok(result)
    }

    async fn purge_observed_terminal(&self, reference: &OperationRef) -> Result<()> {
        let Some(operation) = self.get(reference).await? else {
            return Ok(());
        };
        if !operation.state.is_terminal() {
            return Ok(());
        }
        if operation.kind == OperationKind::Session
            && self
                .current(&reference.session_id)
                .await?
                .is_some_and(|op| op.reference == *reference)
        {
            return Ok(());
        }
        if let Some(parent) = &operation.parent {
            if self
                .get(parent)
                .await?
                .is_some_and(|parent| parent.children.contains(reference))
            {
                return Ok(());
            }
        }
        self.kv.purge(reference.key()).await?;
        Ok(())
    }

    pub fn from_store(kv: kv::Store) -> Self {
        Self { kv }
    }

    pub async fn record_coverage(
        &self,
        reference: &OperationRef,
        owner: &Owner,
        through: u64,
        cancelled: bool,
    ) -> Result<()> {
        self.mutate(reference, |op| {
            op.check_owner(owner)?;
            op.covered_through = op.covered_through.max(through);
            op.cancel_recorded |= cancelled;
            Ok(())
        })
        .await?;
        Ok(())
    }

    /// Freeze normal admissions only if the owner has consumed every reservation.
    pub async fn seal(&self, reference: &OperationRef, owner: &Owner) -> Result<bool> {
        let op = self
            .mutate(reference, |op| {
                op.check_owner(owner)?;
                if op.state == OperationState::Running && op.admissions_covered(op.covered_through)
                {
                    op.sealed = true;
                }
                Ok(())
            })
            .await?;
        Ok(op.sealed)
    }

    pub async fn purge_session(&self, session: &str) -> Result<()> {
        let prefix = format!("sessions/{session}/");
        let mut keys = self.kv.keys().await?;
        while let Some(key) = keys.next().await {
            let key = key?;
            if key.starts_with(&prefix) {
                self.kv.purge(key).await?;
            }
        }
        Ok(())
    }
}

fn current_key(session: &str) -> String {
    format!("sessions/{session}/current")
}
