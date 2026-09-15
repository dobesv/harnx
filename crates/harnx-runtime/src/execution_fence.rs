//! Creation-time worker authority. Every positive boundary goes through gate CAS.
use anyhow::Result;
use harnx_execution_control::{
    CommitAction, CommitReceipt, ExecutionContext, ExecutionStore, GateAction, OperationKind,
    OperationRef, OutputKind, WorkRegistration,
};
use serde_json::Value;

#[derive(Clone)]
pub struct GenerationFence(std::sync::Arc<ExecutionAuthority>);

/// Immutable binding shared by a worker's cloned configs and callbacks.
pub struct ExecutionAuthority {
    pub(crate) store: ExecutionStore,
    pub(crate) context: ExecutionContext,
    events_stopped: std::sync::atomic::AtomicBool,
}

impl std::ops::Deref for GenerationFence {
    type Target = ExecutionAuthority;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl GenerationFence {
    pub fn new(store: ExecutionStore, context: ExecutionContext) -> Self {
        Self(std::sync::Arc::new(ExecutionAuthority {
            store,
            context,
            events_stopped: false.into(),
        }))
    }

    pub(crate) fn events_stopped(&self) -> bool {
        self.events_stopped
            .load(std::sync::atomic::Ordering::Acquire)
    }

    pub(crate) fn stop_events(&self) {
        self.events_stopped
            .store(true, std::sync::atomic::Ordering::Release);
    }

    pub(crate) async fn check(&self, boundary: &str) -> Result<()> {
        self.admit(boundary).await.map(drop)
    }

    pub(crate) async fn admit(&self, boundary: &str) -> Result<CommitReceipt> {
        Box::pin(self.store.commit_if_admissible(
            &self.context,
            CommitAction {
                id: uuid::Uuid::now_v7().to_string(),
                kind: GateAction::AdmitWork {
                    input: Value::String(boundary.into()),
                },
            },
        ))
        .await
    }

    pub(crate) async fn output(&self, kind: OutputKind, payload: Value) -> Result<CommitReceipt> {
        Box::pin(self.store.commit_blob_output(
            &self.context,
            harnx_execution_control::CommittedOutput {
                id: uuid::Uuid::now_v7().to_string(),
                kind,
                payload,
            },
        ))
        .await
    }

    /// Local/provider-independent work has no physical graph node. Its gate
    /// registration still races stop, including synthetic tools such as handoff.
    pub(crate) async fn start(&self, input: Value) -> Result<()> {
        // Tool arguments can exceed the gate's bounded action size. StartWork
        // refers to exact committed input, never an unspecified later request.
        let input =
            serde_json::json!({"committed_input": self.output(OutputKind::Progress, input).await?});
        let id = uuid::Uuid::now_v7().to_string();
        Box::pin(self.store.commit_if_admissible(
            &self.context,
            CommitAction {
                id: id.clone(),
                kind: GateAction::StartWork {
                    child: WorkRegistration {
                        operation: OperationRef::new(&self.context.generation().session_id, id),
                        kind: OperationKind::Tool,
                        owner: self.context.owner().clone(),
                    },
                    input,
                },
            },
        ))
        .await?;
        Ok(())
    }

    pub(crate) fn check_blocking(&self, boundary: &str) -> Result<()> {
        let fence = self.clone();
        let boundary = boundary.to_string();
        block_on_io(async move { fence.check(&boundary).await })
    }
}

/// Local cancellation is a fast reject, never positive authorization. Callers
/// keep this fence, not a pointer to whichever config/generation is current later.
pub(crate) async fn check(
    fence: Option<&GenerationFence>,
    abort: &harnx_core::abort::AbortSignal,
    boundary: &str,
) -> Result<()> {
    anyhow::ensure!(!abort.aborted(), "interrupted by user");
    if let Some(fence) = fence {
        fence.check(boundary).await?;
    }
    anyhow::ensure!(!abort.aborted(), "interrupted by user");
    Ok(())
}

pub(crate) fn tool_boundary(
    fence: Option<GenerationFence>,
) -> Option<std::sync::Arc<harnx_engine::tool::WorkBoundaryFn>> {
    fence.map(|fence| {
        std::sync::Arc::new(move |boundary, call| {
            let fence = fence.clone();
            Box::pin(async move {
                match boundary {
                    harnx_engine::tool::WorkBoundary::Start => {
                        fence.start(serde_json::to_value(call)?).await
                    }
                    harnx_engine::tool::WorkBoundary::Accept => fence.check("tool-output").await,
                }
            })
                as std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>
        }) as std::sync::Arc<harnx_engine::tool::WorkBoundaryFn>
    })
}

/// Keep broker polling off the deep synchronous model/tool persistence stack.
/// The join is owned, so dropping the waiter cannot detach unfinished I/O.
pub(crate) fn block_on_io<T: Send + 'static>(
    future: impl std::future::Future<Output = Result<T>> + Send + 'static,
) -> Result<T> {
    use tracing::Instrument;
    let task = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(
        future.instrument(tracing::Span::current()),
    ));
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(task))?
}
