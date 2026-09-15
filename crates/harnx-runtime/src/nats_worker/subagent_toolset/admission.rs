use super::*;
use crate::execution_fence::GenerationFence;

#[derive(Clone)]
pub(super) struct Admission {
    pub fence: Option<GenerationFence>,
    pub cancel: CancellationToken,
    #[cfg(test)]
    barrier: Option<std::sync::Arc<AdmissionBarrier>>,
}

impl Admission {
    pub async fn capture(
        toolset: &SubagentToolset,
        context: &ToolInvocationContext,
        cancel: CancellationToken,
    ) -> Result<Self, ToolInvokeError> {
        let fence = match &context.execution {
            Some(execution) => {
                let store = harnx_execution_control::ExecutionStore::from_store(
                    toolset
                        .jetstream
                        .get_key_value(harnx_execution_control::BUCKET)
                        .await
                        .map_err(|error| ToolInvokeError::Fatal(error.to_string()))?,
                );
                Some(GenerationFence::new(store, execution.clone()))
            }
            None if context.operation.is_some() => {
                return Err(ToolInvokeError::Fatal(
                    "sub-agent invocation has no generation authority".into(),
                ))
            }
            None => None,
        };
        let admission = Self {
            fence,
            cancel,
            #[cfg(test)]
            barrier: toolset.admission_barrier.clone(),
        };
        admission.check("child-registration").await?;
        Ok(admission)
    }

    pub async fn check(&self, boundary: &str) -> Result<(), ToolInvokeError> {
        self.check_local()?;
        if let Some(fence) = &self.fence {
            fence.check(boundary).await.map_err(error)?;
        }
        self.check_local()
    }

    pub async fn start(&self, input: Value) -> Result<(), ToolInvokeError> {
        self.check_local()?;
        #[cfg(test)]
        if let Some(barrier) = &self.barrier {
            barrier.entered.wait().await;
            barrier.release.wait().await;
        }
        if let Some(fence) = &self.fence {
            fence.start(input).await.map_err(error)?;
        }
        self.check_local()
    }

    fn check_local(&self) -> Result<(), ToolInvokeError> {
        if self.cancel.is_cancelled() {
            return Err(ToolInvokeError::Fatal("sub-agent tool call aborted".into()));
        }
        Ok(())
    }
}

pub(super) fn error(error: anyhow::Error) -> ToolInvokeError {
    match error.downcast::<harnx_execution_control::Interrupted>() {
        Ok(interrupted) => ToolInvokeError::Interrupted(Box::new(interrupted)),
        Err(error) => ToolInvokeError::Fatal(format!("{error:#}")),
    }
}

pub(super) async fn append_started(
    toolset: &SubagentToolset,
    destination: (&str, &str),
    entry: &SessionLogEntry,
    admission: &Admission,
) -> Result<bool, ToolInvokeError> {
    let (parent, invocation) = destination;
    admission.check("subagent-transcript").await?;
    let log = NatsSessionLog::new(toolset.jetstream.clone(), parent);
    let Some(fence) = &admission.fence else {
        return crate::nats_session::append_invocation_entry(&log, entry, invocation)
            .await
            .map(|(_, inserted)| inserted)
            .map_err(error);
    };
    let through = fence
        .admit("subagent-start-projection")
        .await
        .map_err(error)?;
    log.project_through(fence, &through).await.map_err(error)?;
    let entries = log.load_events_latest_async().await.map_err(error)?;
    if entries.iter().any(|(_, entry)| {
        matches!(entry, SessionLogEntry::SubAgentStarted {
        invocation_id: Some(id), ..
    } if id == invocation)
    }) {
        return Ok(false);
    }
    log.append_output(fence, entry, None).await.map_err(error)?;
    Ok(true)
}

#[cfg(test)]
pub(super) struct AdmissionBarrier {
    entered: tokio::sync::Barrier,
    release: tokio::sync::Barrier,
}

#[cfg(test)]
#[path = "admission_tests.rs"]
mod tests;
