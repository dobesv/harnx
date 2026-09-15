//! Compact only after a successful root CAS fences every captured candidate.
//! Readers of a collected old snapshot fail closed and can reload the new head.

use super::{
    index::{Index, Node, NodeRef},
    ledger::{decision_key, Proof},
    types::*,
};
use crate::{ExecutionStore, OperationRef};
use anyhow::{ensure, Context, Result};
use futures_util::StreamExt;
use std::collections::{BTreeMap, BTreeSet};

impl ExecutionStore {
    /// Checkpoint logical state, then collect obsolete index paths and losing
    /// candidates. No committed payload is collected: replay/idempotency still
    /// needs it until session deletion. Sink-specific retention watermarks belong
    /// to the projector adapters, not to this mechanism's garbage collector.
    pub async fn checkpoint_gate(&self, root: &OperationRef) -> Result<GateCheckpoint> {
        loop {
            let snapshot = self.gate_snapshot(root).await?;
            let source = snapshot.index(self);
            // Capture keys BEFORE CAS. A candidate born after this checkpoint
            // must never be collected just because it hasn't committed yet.
            let garbage = self.gate_keys(&source.prefix).await?;
            let mut destination = snapshot.index(self);
            destination.epoch = uuid::Uuid::now_v7().to_string();
            let retained = copy_index(&source, &mut destination).await?;
            let mut head = snapshot.head.clone();
            head.epoch = destination.epoch;
            head.state = destination.root;
            head.checkpoint = head.tip.as_ref().context("empty gate")?.sequence;
            if !self.cas_gate(snapshot.revision, &head).await? {
                tokio::task::yield_now().await;
                continue;
            }
            self.collect_gate_garbage(garbage, &retained).await?;
            return Ok(GateCheckpoint {
                gate_root: root.clone(),
                epoch: head.epoch,
                through_sequence: head.checkpoint,
            });
        }
    }

    pub async fn projection_cursor(
        &self,
        root: &OperationRef,
        projector: &str,
    ) -> Result<Option<CommitReceipt>> {
        validate_id(projector)?;
        self.gate_snapshot(root)
            .await?
            .index(self)
            .get(&format!("projector/{projector}"))
            .await
    }

    async fn collect_gate_garbage(
        &self,
        keys: Vec<String>,
        retained: &BTreeSet<String>,
    ) -> Result<()> {
        for key in keys {
            if !retained.contains(&key) {
                self.kv.purge(key).await?;
            }
        }
        Ok(())
    }

    async fn gate_keys(&self, prefix: &str) -> Result<Vec<String>> {
        let mut keys = self.kv.keys().await?;
        let mut selected = Vec::new();
        while let Some(key) = keys.next().await {
            let key = key?;
            if key.starts_with(&format!("{prefix}nodes/"))
                || key.starts_with(&format!("{prefix}decisions/"))
            {
                selected.push(key);
            }
        }
        Ok(selected)
    }
}

async fn copy_index(source: &Index<'_>, destination: &mut Index<'_>) -> Result<BTreeSet<String>> {
    let root = source.root.clone().context("empty gate index")?;
    let mut stack = vec![(root.clone(), false)];
    let mut copied = BTreeMap::new();
    let mut retained = BTreeSet::new();
    let mut seen = BTreeSet::new();
    while let Some((reference, visited)) = stack.pop() {
        let key = source.node_key(&reference)?;
        let mut node = source.node(&reference).await?;
        if visited {
            rewrite_children(&mut node, source, &copied)?;
            copied.insert(key, destination.save(&node).await?);
            continue;
        }
        ensure!(seen.insert(key), "gate index cycle");
        match &node {
            Node::Leaf { key, value } => {
                if key.starts_with("proof/") {
                    let proof: Proof = serde_json::from_value(value.clone())?;
                    retained.insert(decision_key(&proof)?);
                }
            }
            Node::Branch { zero, one, .. } => {
                stack.push((reference.clone(), true));
                stack.push((one.clone(), false));
                stack.push((zero.clone(), false));
                continue;
            }
        }
        copied.insert(source.node_key(&reference)?, destination.save(&node).await?);
    }
    destination.root = Some(
        copied
            .remove(&source.node_key(&root)?)
            .context("checkpoint root missing")?,
    );
    Ok(retained)
}

fn rewrite_children(
    node: &mut Node,
    source: &Index<'_>,
    copied: &BTreeMap<String, NodeRef>,
) -> Result<()> {
    if let Node::Branch { zero, one, .. } = node {
        *zero = copied
            .get(&source.node_key(zero)?)
            .context("checkpoint child missing")?
            .clone();
        *one = copied
            .get(&source.node_key(one)?)
            .context("checkpoint child missing")?
            .clone();
    }
    Ok(())
}
