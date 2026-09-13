//! An invocation retains ownership until its future and registered children stop.
use anyhow::{ensure, Context, Result};
use futures_util::StreamExt;
use harnx_execution_control::{ExecutionStore, Operation, OperationKind, OperationRef, Owner};
use harnx_toolset::{CancellationGuarantee, ToolInvokeError, ToolRequest};
use serde_json::Value;
use std::{future::Future, time::Duration};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

const COMPLETION_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub(super) struct ActiveCall {
    pub reference: OperationRef,
    pub cancel: CancellationToken,
    pub stopped: watch::Receiver<bool>,
}

pub(super) struct InvocationExecution {
    store: ExecutionStore,
    pub reference: OperationRef,
    owner: Owner,
    watch: harnx_nats_common::recovery::KvUpdates,
}

impl InvocationExecution {
    pub async fn claim(
        store: &ExecutionStore,
        request: &ToolRequest,
        server: &str,
    ) -> Result<Self> {
        ensure!(
            request.operation_id == request.call_id,
            "operation ID must match attested call ID"
        );
        let session = request
            .parent_session_id
            .as_deref()
            .unwrap_or(&request.call_id);
        let reference = OperationRef::new(session, &request.operation_id);
        if store.get(&reference).await?.is_none() {
            // Standalone tool clients have no worker execution. A worker's calls
            // must already be registered: never attach a late call to a new turn.
            ensure!(
                store.current(session).await?.is_none(),
                "tool operation was not registered by its execution owner"
            );
            store
                .create(&Operation::preparing(
                    reference.clone(),
                    OperationKind::Tool,
                    None,
                ))
                .await?;
        }
        let owner = Owner::invocation(server);
        match &request.replay {
            Some(parent_owner) => {
                store
                    .claim_replay(&reference, parent_owner, owner.clone())
                    .await?;
            }
            None => {
                store.claim(&reference, owner.clone()).await?;
            }
        }
        let watch = store.watch().await?;
        Ok(Self {
            store: store.clone(),
            reference,
            owner,
            watch,
        })
    }

    async fn begin_cancel(&mut self, cancel: &CancellationToken) -> Result<()> {
        // Signal first: broker latency must not delay local cooperative cleanup.
        cancel.cancel();
        self.store
            .cancel_operation(&self.reference, None, false)
            .await?;
        self.store.quiesce(&self.reference, &self.owner).await?;
        Ok(())
    }

    pub async fn invoke(
        mut self,
        cancel: CancellationToken,
        guarantee: CancellationGuarantee,
        future: impl Future<Output = Result<Value, ToolInvokeError>>,
        stopped: watch::Sender<bool>,
    ) -> Result<Value, ToolInvokeError> {
        let result = self.run(&cancel, guarantee, future).await;
        // A child can lose its lease without recording owner_stopped. Waiting
        // forever here used to hide even an already-produced error/answer from
        // the caller. Bound confirmation, while retaining the durable blocker
        // and withholding the stopped acknowledgement when cleanup is unknown.
        let completion = self.finish().await;
        completion.map_err(|error| {
            let detail = match &result {
                Err(original) => format!("; invocation error: {original}"),
                Ok(_) => {
                    "; invocation returned a result but descendant shutdown could not be confirmed"
                        .to_string()
                }
            };
            ToolInvokeError::Fatal(format!("tool shutdown unconfirmed: {error:#}{detail}"))
        })?;
        stopped.send_replace(true);
        result
    }

    async fn run(
        &mut self,
        cancel: &CancellationToken,
        guarantee: CancellationGuarantee,
        future: impl Future<Output = Result<Value, ToolInvokeError>>,
    ) -> Result<Value, ToolInvokeError> {
        if self.store.check_ancestors(&self.reference).await.is_err() {
            self.begin_cancel(cancel).await.map_err(control_error)?;
            return Err(ToolInvokeError::Recoverable(
                "tool cancelled before activation".into(),
            ));
        }
        tokio::pin!(future);
        let mut cancelling = false;
        let mut progress = tokio::time::interval(Duration::from_millis(100));
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled(), if !cancelling => {
                    if let Err(error) = self.begin_cancel(cancel).await {
                        log::warn!("cannot persist tool cancellation: {error:#}");
                    }
                    cancelling = true;
                    if guarantee == CancellationGuarantee::HardOnDrop {
                        return Err(ToolInvokeError::Recoverable("tool cancelled".into()));
                    }
                }
                result = &mut future => return result,
                update = self.watch.next(), if !cancelling => {
                    if !matches!(update, Some(Ok(_))) || self.store.check_ancestors(&self.reference).await.is_err() {
                        cancel.cancel();
                    }
                }
                _ = progress.tick(), if cancelling => {
                    // A stubborn cooperative future stays alive; its durable
                    // blocker becomes static after five seconds and may recover.
                    if let Err(error) = self.store.status(&self.reference).await {
                        log::warn!("cannot reconcile cancelling tool: {error:#}");
                    }
                }
            }
        }
    }

    async fn finish(&mut self) -> Result<()> {
        let operation = self
            .store
            .owner_stopped(&self.reference, &self.owner)
            .await?;
        if operation.state.is_terminal() {
            return Ok(());
        }
        tokio::time::timeout(COMPLETION_TIMEOUT, self.wait_for_children())
            .await
            .context("timed out waiting for registered children to stop")?
    }

    async fn wait_for_children(&mut self) -> Result<()> {
        // A completed invocation may still be waiting for its child's lease
        // release. Normal cleanup must not turn that success into cancellation.
        loop {
            let operation = self.store.status(&self.reference).await?;
            if operation.state.is_terminal() {
                return Ok(());
            }
            ensure!(
                operation.state != harnx_execution_control::OperationState::Unconfirmed,
                "{}",
                operation
                    .blocker
                    .context("registered child has not stopped")?
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

fn control_error(error: anyhow::Error) -> ToolInvokeError {
    ToolInvokeError::Fatal(format!("tool execution control failed: {error:#}"))
}

/// Cached replies and rejected calls own no new invocation work. Reconcile a
/// separately registered alias only after the original cached future returned.
pub(super) async fn complete_without_invocation(
    context: &super::ToolRequestContext,
    request: &ToolRequest,
) -> Result<()> {
    let session = request
        .parent_session_id
        .as_deref()
        .unwrap_or(&request.call_id);
    let reference = OperationRef::new(session, &request.operation_id);
    let Some(operation) = context.execution_store.get(&reference).await? else {
        return Ok(());
    };
    if operation.owner.is_some() || operation.state.is_terminal() {
        return Ok(());
    }
    let owner = Owner::invocation(&context.server_identity);
    context
        .execution_store
        .claim(&reference, owner.clone())
        .await?;
    context
        .execution_store
        .owner_stopped(&reference, &owner)
        .await?;
    Ok(())
}
