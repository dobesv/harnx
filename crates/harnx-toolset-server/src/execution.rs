//! The owner task outlives its reply. Acceptance never waits for handler cleanup.
use anyhow::{ensure, Result};
use futures_util::StreamExt;
use harnx_execution_control::{
    ExecutionContext, ExecutionStore, InterruptScope, Interrupted, OperationRef, Owner,
};
use harnx_toolset::{CancellationGuarantee, ToolInvokeError, ToolRequest};
use serde_json::Value;
use std::{future::Future, time::Duration};
use tokio::sync::oneshot;

#[cfg(test)]
#[path = "execution_tests.rs"]
mod tests;
use tokio_util::sync::CancellationToken;

const CLEANUP_BUDGET: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub(super) struct ActiveCall {
    pub producer: ExecutionContext,
    pub cancel: CancellationToken,
}

pub(super) struct InvocationExecution {
    store: ExecutionStore,
    pub reference: OperationRef,
    pub producer: ExecutionContext,
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
        let producer = crate::reply_fence::claim_producer(store, request, server).await?;
        let owner = producer.owner().clone();
        match &request.replay {
            Some(parent_owner) => {
                store.claim_replay(&reference, parent_owner, owner).await?;
            }
            None => {
                store.claim(&reference, owner).await?;
            }
        }
        crate::reply_fence::admit_producer(store, request, &producer).await?;
        let watch = store.watch().await?;
        Ok(Self {
            store: store.clone(),
            reference,
            producer,
            watch,
        })
    }

    async fn interruption(&mut self) -> Result<Value, ToolInvokeError> {
        let acceptance = async {
            if let Some(stop) = self
                .store
                .gate_stop(self.producer.gate_root(), &self.reference)
                .await?
            {
                return Ok(stop);
            }
            self.store
                .interrupt(
                    &InterruptScope {
                        gate_root: self.producer.gate_root().clone(),
                        operation: self.reference.clone(),
                        reason: "tool invocation cancelled".into(),
                    },
                    &uuid::Uuid::now_v7().to_string(),
                )
                .await
        };
        match tokio::time::timeout(Duration::from_secs(2), acceptance).await {
            Ok(Ok(stop)) => Err(ToolInvokeError::Interrupted(Box::new(Interrupted { stop }))),
            result => Err(ToolInvokeError::Fatal(format!(
                "tool interrupted; stop acceptance unknown: {result:?}"
            ))),
        }
    }

    /// Called only in the server-lifetime CleanupTasks set. It owns the future,
    /// while the request path owns only a oneshot receiver.
    pub async fn invoke(
        mut self,
        cancel: CancellationToken,
        guarantee: CancellationGuarantee,
        future: impl Future<Output = Result<Value, ToolInvokeError>>,
        reply: oneshot::Sender<Result<Value, ToolInvokeError>>,
    ) {
        let cleanup = harnx_toolset::cleanup::InvocationCleanup::default();
        let mut future =
            Box::pin(harnx_toolset::cleanup::INVOCATION_CLEANUP.scope(cleanup.clone(), future));
        let mut started = false;
        let result = {
            let mut observed = Box::pin(std::future::poll_fn(|cx| {
                started = true;
                future.as_mut().poll(cx)
            }));
            self.run(&cancel, observed.as_mut()).await
        };
        let interrupted = result.is_none();
        let result = match result {
            Some(result) => result,
            None => self.interruption().await,
        };
        let _ = reply.send(result);
        if interrupted {
            // Project bookkeeping after sending the logical outcome. The gate
            // stop already fences replies, admissions and descendants.
            let _ = self
                .store
                .cancel_operation(&self.reference, None, false)
                .await;
            // Retain work already started, but never start an unpolled handler
            // merely to clean it up after acceptance.
            if started && guarantee == CancellationGuarantee::Cooperative {
                self.drain_handler(future.as_mut()).await;
            }
        }
        drop(future);
        self.finish_owner(cleanup.last_error()).await;
    }

    async fn run<F>(
        &mut self,
        cancel: &CancellationToken,
        mut future: std::pin::Pin<&mut F>,
    ) -> Option<Result<Value, ToolInvokeError>>
    where
        F: Future<Output = Result<Value, ToolInvokeError>>,
    {
        if self.store.check_ancestors(&self.reference).await.is_err() {
            cancel.cancel();
        }
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return None,
                result = &mut future => return Some(result),
                update = self.watch.next() => {
                    if !matches!(update, Some(Ok(_))) || self.store.check_ancestors(&self.reference).await.is_err() {
                        cancel.cancel();
                    }
                }
            }
        }
    }

    async fn drain_handler<F>(&mut self, mut future: std::pin::Pin<&mut F>)
    where
        F: Future<Output = Result<Value, ToolInvokeError>>,
    {
        if tokio::time::timeout(CLEANUP_BUDGET, &mut future)
            .await
            .is_err()
        {
            let _ = self
                .store
                .unconfirm_cleanup_owner(
                    &self.producer,
                    "tool handler has not stopped within cleanup budget".into(),
                )
                .await;
            // Retain and poll a cooperative future, even forever. Dropping a
            // JoinHandle would detach it; aborting spawn_blocking cannot stop it.
            let _ = future.await;
        }
    }

    async fn finish_owner(&mut self, unconfirmed: Option<String>) {
        let mut backoff = Duration::from_millis(100);
        loop {
            let result = match &unconfirmed {
                Some(reason) => {
                    self.store
                        .unconfirm_cleanup_owner(&self.producer, reason.clone())
                        .await
                }
                None => self.store.finish_cleanup_owner(&self.producer).await,
            };
            match result {
                Ok(()) => return,
                Err(error) => log::warn!("tool cleanup evidence not recorded; retrying: {error:#}"),
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    }
}

/// Rejected calls own no new invocation work. Never take over an existing owner.
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
