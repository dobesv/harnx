//! Idempotent projection of generation-bound worker metadata, ordered with its transcript.
use super::*;
use anyhow::{ensure, Context, Result};
use harnx_core::execution_context::{
    ExecutionContextExtension, ExecutionContextObservation, EXECUTION_CONTEXT_NAMESPACE,
};
use harnx_execution_control::{CommitReceipt, CommittedDecision, ExecutionStore};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub(crate) enum MetadataOutput {
    Title {
        title: String,
        manual: bool,
        tokens: usize,
    },
    Overrides(SessionOverrides),
    Override(SessionOverrideUpdate),
    Variables(harnx_core::agent_config::AgentVariables),
    ExecutionContexts(Vec<ExecutionContextObservation>),
}

impl MetadataOutput {
    fn apply(&self, metadata: &mut SessionMetadata) -> Result<()> {
        match self {
            Self::Title {
                title,
                manual,
                tokens,
            } => {
                metadata.title.value = Some(title.clone());
                metadata.title.manual = *manual;
                metadata.title.last_updated_tokens = *tokens;
            }
            Self::Overrides(overrides) => metadata.overrides = overrides.clone(),
            Self::Override(update) => update.apply(&mut metadata.overrides),
            Self::Variables(variables) => metadata.variables = variables.clone(),
            Self::ExecutionContexts(observations) => merge_contexts(metadata, observations)?,
        }
        Ok(())
    }
}

fn merge_contexts(
    metadata: &mut SessionMetadata,
    observations: &[ExecutionContextObservation],
) -> Result<()> {
    let mut extension = match metadata.extensions.get(EXECUTION_CONTEXT_NAMESPACE) {
        Some(value) => ExecutionContextExtension::from_value(value.clone())?,
        None => ExecutionContextExtension::default(),
    };
    for observation in observations {
        observation.validate()?;
        extension.merge(observation.clone());
    }
    metadata.extensions.insert(
        EXECUTION_CONTEXT_NAMESPACE.into(),
        serde_json::to_value(extension)?,
    );
    Ok(())
}

impl SessionMetadataStore {
    pub(crate) async fn project_output(
        &self,
        store: &ExecutionStore,
        decision: &CommittedDecision,
    ) -> Result<()> {
        let payload = store.committed_output_payload(&decision.receipt).await?;
        let output: MetadataOutput =
            serde_json::from_value(payload).context("decode committed metadata")?;
        let session = &decision.context.generation().session_id;
        self.patch_guarded_if_changed(session, store::PatchGuard::default(), |metadata| {
            if already_projected(metadata.worker_projection.as_ref(), &decision.receipt)? {
                return Ok(false);
            }
            output.apply(metadata)?;
            metadata.worker_projection = Some(decision.receipt.clone());
            Ok(true)
        })
        .await?;
        Ok(())
    }
}

fn already_projected(previous: Option<&CommitReceipt>, receipt: &CommitReceipt) -> Result<bool> {
    let Some(previous) = previous else {
        return Ok(false);
    };
    ensure!(
        previous.gate_root == receipt.gate_root,
        "metadata projector gate changed"
    );
    Ok(previous.sequence >= receipt.sequence)
}
