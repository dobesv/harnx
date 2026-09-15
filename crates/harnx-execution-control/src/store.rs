use crate::*;
use anyhow::{bail, ensure, Context, Result};
use async_nats::jetstream::{self, kv, stream};
use futures_util::StreamExt;
use std::collections::{BTreeMap, BTreeSet};

#[path = "retention.rs"]
mod retention;
pub use retention::RecoveryHistory;

#[derive(Clone, Debug)]
pub struct ExecutionStore {
    pub(crate) kv: kv::Store,
}

impl ExecutionStore {
    pub async fn watch(&self) -> Result<harnx_nats_common::recovery::KvUpdates> {
        harnx_nats_common::recovery::kv_updates(self.kv.clone()).await
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

    pub async fn current(&self, session: &str) -> Result<Option<Operation>> {
        let Some(value) =
            harnx_nats_common::recovery::read(|| self.kv.get(current_key(session))).await?
        else {
            return Ok(None);
        };
        let reference: OperationRef = serde_json::from_slice(&value)?;
        Ok(Some(self.get(&reference).await?.context(
            "current execution record missing; cancellation unconfirmed",
        )?))
    }

    /// Install after logical completion/interruption, without awaiting cleanup
    /// of explicitly interrupted work. Legacy cancellation still waits.
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
                if !self.can_replace_session_generation(&current).await? {
                    ensure!(
                        current.allows_continuation(),
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
            let candidate = Operation::session_generation(
                OperationRef::new(session, id),
                parent.clone(),
                previous.clone(),
            );
            self.create(&candidate).await?;
            let payload = serde_json::to_vec(&candidate.reference)?;
            match self.kv.update(&key, payload.into(), revision).await {
                Ok(_) => {
                    if let Some(previous) = &previous {
                        self.retire_observed(previous).await?;
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
                    self.retire_observed(&candidate.reference).await?;
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
            let previous_stop = operation.stop_decision.clone();
            let gate_registration = operation.gate_registration.clone();
            update(&mut operation)?;
            operation.check_stop_decision(previous_stop.as_ref())?;
            if let Some(registration) = gate_registration {
                ensure!(
                    operation.gate_registration.as_ref() == Some(&registration),
                    "gate registration cannot be replaced"
                );
            }
            if serde_json::to_vec(&operation)? == before {
                return Ok(operation);
            }
            match harnx_nats_common::cas::update(
                &self.kv,
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
        let operation = self
            .mutate(reference, |operation| {
                ensure!(!operation.is_stopped(), "execution already stopped");
                if let Some(previous) = &operation.owner {
                    ensure!(
                        previous == &owner
                            || (operation.kind == OperationKind::Session
                                && owner.fence > previous.fence),
                        "stale execution owner"
                    );
                }
                if let Some(previous) = &operation.owner {
                    operation.owner_fences.insert(previous.fence);
                }
                operation.owner_fences.insert(owner.fence);
                operation.owner = Some(owner.clone());
                operation.owner_stopped = false;
                operation.sealed = false;
                if operation.state == OperationState::Preparing {
                    operation.transition(OperationState::Running)?;
                }
                Ok(())
            })
            .await?;
        Box::pin(self.bridge_owner(&operation, &owner)).await?;
        Ok(operation)
    }

    /// Replay is a tool-server decision, authorized by the current session
    /// owner. Keep the same operation and children so cancellation still reaches
    /// work started by the original invocation; never reopen terminal work.
    pub async fn claim_replay(
        &self,
        reference: &OperationRef,
        parent_owner: &Owner,
        owner: Owner,
    ) -> Result<Operation> {
        let previous = self
            .get(reference)
            .await?
            .context("replayed execution missing")?;
        let parent = previous
            .parent
            .as_ref()
            .context("replay requires a session owner")?;
        self.get(parent)
            .await?
            .context("replay parent missing")?
            .check_owner(parent_owner)?;
        // Registration itself has two writes. Repair a missing parent edge
        // before permitting work, even when the child record already exists.
        self.register(reference, parent).await?;
        let result = self
            .mutate(reference, |operation| {
                ensure!(
                    operation.kind == OperationKind::Tool && operation.allows_continuation(),
                    "tool execution cannot be replayed"
                );
                ensure!(
                    operation.parent == previous.parent && operation.owner == previous.owner,
                    "tool owner changed during replay"
                );
                if let Some(previous) = &operation.owner {
                    operation.owner_fences.insert(previous.fence);
                }
                operation.owner_fences.insert(owner.fence);
                operation.owner = Some(owner.clone());
                operation.owner_stopped = false;
                operation.sealed = false;
                if operation.state == OperationState::Preparing {
                    operation.transition(OperationState::Running)?;
                }
                Ok(())
            })
            .await?;
        let validation = async {
            self.get(parent)
                .await?
                .context("replay parent missing")?
                .check_owner(parent_owner)?;
            self.check_ancestors(reference).await
        }
        .await;
        if let Err(error) = validation {
            // No handler has started under this owner. Restore the prior owner
            // only if another replay has not already claimed the operation.
            self.mutate(reference, |operation| {
                restore_replay_claim(operation, &previous, &owner)
            })
            .await?;
            return Err(error);
        }
        Ok(result)
    }

    pub async fn reserve_prompt(&self, reference: &OperationRef, message_id: &str) -> Result<()> {
        let operation = self
            .mutate(reference, |operation| {
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
        if operation.gate_registration.is_some() {
            let context = self.activate_gate(reference).await?;
            self.commit_if_admissible(
                &context,
                CommitAction {
                    id: format!("prompt-{message_id}"),
                    kind: GateAction::AdmitWork {
                        input: serde_json::json!({"prompt": message_id}),
                    },
                },
            )
            .await?;
        }
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

    /// Make an unconfirmed cancellation terminal so a session can admit a new
    /// execution. This is an explicit operator override: owners that vanished
    /// without acknowledging cleanup may still be running.
    pub async fn abandon_unconfirmed(
        &self,
        session: &str,
        expected_execution_id: &str,
    ) -> Result<CancelReceipt> {
        let Some(current) = self.current(session).await? else {
            return Ok(CancelReceipt::idle());
        };
        if expected_execution_id != current.reference.execution_id {
            return Ok(CancelReceipt::idle());
        }

        let current = self.status(&current.reference).await?;
        if current.state.is_lifecycle_terminal() {
            return Ok(CancelReceipt::from_operation(&current, false));
        }
        ensure!(
            current.state == OperationState::Unconfirmed,
            "cancellation is {:?}; only unconfirmed cancellation can be abandoned",
            current.state
        );

        let postorder = verified_postorder(self, &current.reference).await?;
        let result = abandon_postorder(self, &current.reference, postorder).await?;
        Ok(CancelReceipt::from_operation(&result, false))
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
        let operation = self
            .mutate(reference, |operation| operation.request_cancel(&id, retry))
            .await?;
        Box::pin(self.bridge_cancel(&operation, &id)).await?;
        Ok(operation)
    }

    /// Opt-in model acceptance on one generation's CAS key. Existing cancellation
    /// callers are not migrated yet. This is NOT the tree-wide output commit gate;
    /// later stages must serialize all observable writes with stop acceptance.
    pub async fn accept_interrupt(
        &self,
        reference: &OperationRef,
        cancellation_id: &str,
        reason: &str,
    ) -> Result<Operation> {
        let decision = StopDecision {
            cancellation_id: cancellation_id.into(),
            accepted_at: chrono::Utc::now(),
            reason: reason.into(),
        };
        self.mutate(reference, |operation| {
            ensure!(
                operation.gate_registration.is_none(),
                "gated execution requires tree interrupt"
            );
            operation.accept_interrupt(decision.clone())
        })
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
        let mut watch = self.watch().await?;
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
                operation.allows_continuation(),
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
        if operation.state.is_lifecycle_terminal() {
            Box::pin(self.bridge_finish(&operation)).await?;
            return Ok(operation);
        }
        self.status(reference).await
    }

    pub async fn status(&self, reference: &OperationRef) -> Result<Operation> {
        let mut stack = vec![(reference.clone(), None, false, false)];
        let mut seen = BTreeSet::new();
        let mut root = None;
        while let Some((current, parent, ancestor_cancelling, visited)) = stack.pop() {
            let Some(operation) =
                status_operation(self, &current, parent.as_ref(), ancestor_cancelling).await?
            else {
                continue;
            };
            if visited {
                let result = self.reconcile_one(&current).await.map(Some);
                let Some(operation) =
                    reconcile_observed(self, &current, parent.as_ref(), result).await?
                else {
                    continue;
                };
                if current == *reference {
                    root = Some(operation);
                }
                continue;
            }
            ensure!(
                seen.insert(current.clone()),
                "execution graph cycle; cancellation unconfirmed"
            );
            let descendants_cancelling = ancestor_cancelling || operation.state.cancelling();
            stack.push((current.clone(), parent, ancestor_cancelling, true));
            for child in operation.children {
                stack.push((child, Some(current.clone()), descendants_cancelling, false));
            }
        }
        root.context("execution missing")
    }

    async fn reconcile_one(&self, reference: &OperationRef) -> Result<Operation> {
        let operation = self.get(reference).await?.context("execution missing")?;
        if operation.state.is_lifecycle_terminal() {
            return Ok(operation);
        }
        let mut finished = BTreeSet::new();
        let mut cleanup_unconfirmed = false;
        for child in &operation.children {
            if let Some(child) = self.get(child).await?.filter(Operation::can_prune) {
                cleanup_unconfirmed |= !child.cleanup_confirmed();
                finished.insert(child.reference);
            }
        }
        let result = self
            .mutate(reference, |operation| {
                if operation.state.is_lifecycle_terminal() {
                    return Ok(());
                }
                operation.retired_cleanup_unconfirmed |= cleanup_unconfirmed;
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
        Box::pin(self.bridge_finish(&result)).await?;
        for child in finished {
            self.retire_observed(&child).await?;
        }
        Ok(result)
    }

    pub fn from_store(kv: kv::Store) -> Self {
        Self { kv }
    }

    /// Record transcript projection coverage, not stop acceptance or cleanup.
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

/// Snapshot a verified post-order traversal. Descendants must become terminal
/// before their parents so a new root cannot be installed halfway through.
async fn verified_postorder(
    store: &ExecutionStore,
    root: &OperationRef,
) -> Result<Vec<OperationRef>> {
    let mut stack = vec![(root.clone(), None, false)];
    let mut seen = BTreeSet::new();
    let mut postorder = Vec::new();
    while let Some((reference, parent, visited)) = stack.pop() {
        let operation = store
            .get(&reference)
            .await?
            .context("execution descendant missing; cancellation unconfirmed")?;
        ensure!(
            operation.parent.as_ref() == parent.as_ref(),
            "execution parent mismatch; cancellation unconfirmed"
        );
        if visited {
            postorder.push(reference);
            continue;
        }
        ensure!(
            seen.insert(reference.clone()),
            "execution graph cycle; cancellation unconfirmed"
        );
        stack.push((reference.clone(), parent, true));
        for child in operation.children {
            stack.push((child, Some(reference.clone()), false));
        }
    }
    Ok(postorder)
}

async fn abandon_postorder(
    store: &ExecutionStore,
    root: &OperationRef,
    postorder: Vec<OperationRef>,
) -> Result<Operation> {
    let descendants = postorder.clone();
    for reference in postorder {
        store
            .mutate(&reference, Operation::abandon_unconfirmed)
            .await?;
    }
    let result = store
        .get(root)
        .await?
        .context("execution root missing during abandonment")?;
    for reference in descendants
        .into_iter()
        .filter(|reference| reference != root)
    {
        store.retire_observed(&reference).await?;
    }
    Ok(result)
}

async fn status_operation(
    store: &ExecutionStore,
    reference: &OperationRef,
    parent: Option<&OperationRef>,
    ancestor_cancelling: bool,
) -> Result<Option<Operation>> {
    let result = read_status_operation(store, reference, ancestor_cancelling).await;
    reconcile_observed(store, reference, parent, result).await
}

async fn reconcile_observed(
    store: &ExecutionStore,
    reference: &OperationRef,
    parent: Option<&OperationRef>,
    result: Result<Option<Operation>>,
) -> Result<Option<Operation>> {
    if result.is_ok() {
        return result;
    }
    let Some(parent) = parent else {
        return result;
    };
    // Reconciliation includes multiple reads and CAS writes. A different
    // observer can retire this child at any await, including after preflight.
    // Only ignore absence when its parent no longer retains the blocker.
    if store.get(reference).await?.is_none()
        && store
            .get(parent)
            .await?
            .is_some_and(|op| !op.children.contains(reference))
    {
        return Ok(None);
    }
    result
}

async fn read_status_operation(
    store: &ExecutionStore,
    reference: &OperationRef,
    ancestor_cancelling: bool,
) -> Result<Option<Operation>> {
    let mut operation = store
        .get(reference)
        .await?
        .context("execution descendant missing; cancellation unconfirmed")?;
    if ancestor_cancelling && !operation.state.is_lifecycle_terminal() {
        operation = cancel_from_ancestor(store, reference).await?;
    }
    Ok(Some(operation))
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;

/// Persist inherited cancellation even when the descendant owner vanished
/// before observing its ancestor. An ownerless preparing operation never
/// started work, so it can be closed immediately without cleanup.
async fn cancel_from_ancestor(
    store: &ExecutionStore,
    reference: &OperationRef,
) -> Result<Operation> {
    let cancellation_id = uuid::Uuid::now_v7().to_string();
    store
        .mutate(reference, |operation| {
            if operation.state.is_lifecycle_terminal() {
                return Ok(());
            }
            let never_started =
                operation.state == OperationState::Preparing && operation.owner.is_none();
            operation.request_cancel(&cancellation_id, false)?;
            if never_started {
                operation.transition(OperationState::Quiescing)?;
                operation.owner_stopped = true;
                operation.cancel_recorded = true;
                operation.transition(OperationState::Cancelled)?;
            }
            Ok(())
        })
        .await
}

fn current_key(session: &str) -> String {
    format!("sessions/{session}/current")
}

fn restore_replay_claim(
    operation: &mut Operation,
    previous: &Operation,
    owner: &Owner,
) -> Result<()> {
    if operation.owner.as_ref() != Some(owner) || operation.state.is_lifecycle_terminal() {
        return Ok(());
    }
    operation.owner = previous.owner.clone();
    operation.owner_stopped = previous.owner_stopped;
    if operation.allows_continuation() {
        // This was an uncommitted claim; no handler ran under the new owner.
        operation.state = previous.state;
        operation.sealed = previous.sealed;
    } else if previous.owner.is_none() {
        // Cancellation raced validation. Preserve it, but there is no old
        // handler to await for a claim that never started any work.
        operation.owner_stopped = true;
        operation.cancel_recorded = true;
    }
    Ok(())
}
