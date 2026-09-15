//! Large exact outputs live outside bounded gate decisions. Staging is not acceptance.
use super::{ledger::prefix, types::*};
use crate::ExecutionStore;
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

const CHUNK_SIZE: usize = 64 * 1024;

#[derive(Serialize, Deserialize)]
struct Blob {
    sha256: String,
    bytes: usize,
}

impl ExecutionStore {
    /// Commit the digest of immutable, chunked JSON through the same CAS as stop.
    /// Blobs (including losing candidates) remain under session retention. The
    /// receipt authorizes only projection of these exact bytes, never another output.
    pub async fn commit_blob_output(
        &self,
        ctx: &ExecutionContext,
        output: CommittedOutput,
    ) -> Result<CommitReceipt> {
        let CommittedOutput { id, kind, payload } = output;
        ctx.validate()?;
        validate_id(&id)?;
        let bytes = serde_json::to_vec(&payload)?;
        let blob = Blob {
            sha256: digest(&bytes),
            bytes: bytes.len(),
        };
        for (index, chunk) in bytes.chunks(CHUNK_SIZE).enumerate() {
            let key = blob_key(ctx.gate_root(), &blob, index);
            if let Err(error) = self.kv.create(&key, chunk.to_vec().into()).await {
                ensure!(
                    self.kv.get(&key).await?.as_deref() == Some(chunk),
                    "output blob staging failed: {error}"
                );
            }
        }
        self.commit_if_admissible(
            ctx,
            CommitAction {
                id: id.clone(),
                kind: GateAction::CommitOutput {
                    output: CommittedOutput {
                        id,
                        kind,
                        payload: serde_json::to_value(blob)?,
                    },
                },
            },
        )
        .await
    }

    /// Load an exact committed blob, independently of the producer's current
    /// admission. For historical projection only, not execution/consumption.
    pub async fn committed_output_payload(&self, receipt: &CommitReceipt) -> Result<Value> {
        let decision = self.committed_decision(receipt).await?;
        let CommittedAction::Action {
            action:
                CommitAction {
                    kind: GateAction::CommitOutput { output },
                    ..
                },
        } = decision.action
        else {
            anyhow::bail!("commit is not output");
        };
        let blob: Blob = serde_json::from_value(output.payload)?;
        ensure!(
            blob.sha256.len() == 64 && blob.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "invalid output digest"
        );
        let mut bytes = Vec::new();
        for index in 0..blob.bytes.div_ceil(CHUNK_SIZE) {
            bytes.extend_from_slice(
                &self
                    .kv
                    .get(blob_key(&receipt.gate_root, &blob, index))
                    .await?
                    .context("committed output blob missing")?,
            );
        }
        ensure!(bytes.len() == blob.bytes, "output blob length mismatch");
        ensure!(digest(&bytes) == blob.sha256, "output blob digest mismatch");
        Ok(serde_json::from_slice(&bytes)?)
    }
}

fn blob_key(root: &crate::OperationRef, blob: &Blob, index: usize) -> String {
    format!("{}payload/{}/{index}", prefix(root), blob.sha256)
}

fn digest(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut hex = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        write!(&mut hex, "{byte:02x}").expect("writing into a String");
    }
    hex
}
