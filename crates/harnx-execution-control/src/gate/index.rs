//! Immutable crit-bit index. Only paths to changed keys are copied; no KV value
//! grows with the number of operations, stops, or committed outputs in a tree.

use anyhow::{ensure, Context, Result};
use async_nats::jetstream::kv;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub(super) const MAX_RECORD_BYTES: usize = 128 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct NodeRef {
    pub epoch: String,
    pub id: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) enum Node {
    Leaf {
        key: String,
        value: serde_json::Value,
    },
    Branch {
        bit: u16,
        zero: NodeRef,
        one: NodeRef,
    },
}

pub(super) struct Index<'a> {
    pub kv: &'a kv::Store,
    pub prefix: String,
    pub epoch: String,
    pub root: Option<NodeRef>,
}

impl Index<'_> {
    pub async fn get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        let hash = digest(key);
        let mut next = self.root.clone();
        let mut depth = 0;
        while let Some(reference) = next {
            ensure!(depth <= 256, "invalid gate index depth");
            depth += 1;
            match self.node(&reference).await? {
                Node::Leaf { key: saved, value } => {
                    return if saved == key {
                        Ok(Some(serde_json::from_value(value)?))
                    } else {
                        Ok(None)
                    };
                }
                Node::Branch { bit, zero, one } => {
                    next = Some(if branch_bit(&hash, bit)? { one } else { zero });
                }
            }
        }
        Ok(None)
    }

    pub async fn set<T: Serialize>(&mut self, key: &str, value: &T) -> Result<()> {
        let leaf = Node::Leaf {
            key: key.into(),
            value: serde_json::to_value(value)?,
        };
        let Some(root) = self.root.clone() else {
            self.root = Some(self.save(&leaf).await?);
            return Ok(());
        };
        let hash = digest(key);
        let (path, reference, saved) = self.path(root, &hash).await?;
        let mut replacement = self.save(&leaf).await?;
        let prefix = if saved == key {
            path.len()
        } else {
            let bit = differing_bit(&hash, &digest(&saved))?;
            let prefix = path
                .iter()
                .position(|(_, b, _, _)| *b > bit)
                .unwrap_or(path.len());
            let subtree = path.get(prefix).map_or(reference, |entry| entry.0.clone());
            let (zero, one) = if branch_bit(&hash, bit)? {
                (subtree, replacement)
            } else {
                (replacement, subtree)
            };
            replacement = self.save(&Node::Branch { bit, zero, one }).await?;
            prefix
        };
        for (_, bit, zero, one) in path.into_iter().take(prefix).rev() {
            let (zero, one) = if branch_bit(&hash, bit)? {
                (zero, replacement)
            } else {
                (replacement, one)
            };
            replacement = self.save(&Node::Branch { bit, zero, one }).await?;
        }
        self.root = Some(replacement);
        Ok(())
    }

    async fn path(
        &self,
        mut reference: NodeRef,
        hash: &[u8; 32],
    ) -> Result<(Vec<Branch>, NodeRef, String)> {
        let mut path = Vec::new();
        loop {
            ensure!(path.len() <= 256, "invalid gate index depth");
            match self.node(&reference).await? {
                Node::Leaf { key, .. } => return Ok((path, reference, key)),
                Node::Branch { bit, zero, one } => {
                    let next = if branch_bit(hash, bit)? {
                        one.clone()
                    } else {
                        zero.clone()
                    };
                    path.push((reference, bit, zero, one));
                    reference = next;
                }
            }
        }
    }

    pub async fn node(&self, reference: &NodeRef) -> Result<Node> {
        read(self.kv, &self.node_key(reference)?).await
    }

    pub fn node_key(&self, reference: &NodeRef) -> Result<String> {
        validate_token(&reference.epoch)?;
        validate_token(&reference.id)?;
        Ok(format!(
            "{}nodes/{}/{}",
            self.prefix, reference.epoch, reference.id
        ))
    }

    pub async fn save(&self, node: &Node) -> Result<NodeRef> {
        let reference = NodeRef {
            epoch: self.epoch.clone(),
            id: uuid::Uuid::now_v7().to_string(),
        };
        create(self.kv, &self.node_key(&reference)?, node).await?;
        Ok(reference)
    }
}

type Branch = (NodeRef, u16, NodeRef, NodeRef);

fn digest(key: &str) -> [u8; 32] {
    Sha256::digest(key.as_bytes()).into()
}

fn branch_bit(hash: &[u8; 32], bit: u16) -> Result<bool> {
    ensure!(bit < 256, "invalid gate index bit");
    Ok(hash[bit as usize / 8] & (0x80 >> (bit % 8)) != 0)
}

fn differing_bit(a: &[u8; 32], b: &[u8; 32]) -> Result<u16> {
    for (index, (&left, &right)) in a.iter().zip(b).enumerate() {
        let xor = left ^ right;
        if xor != 0 {
            return Ok(index as u16 * 8 + xor.leading_zeros() as u16);
        }
    }
    anyhow::bail!("gate index hash collision")
}

pub(super) fn validate_token(token: &str) -> Result<()> {
    uuid::Uuid::parse_str(token).context("invalid gate record identity")?;
    Ok(())
}

pub(super) async fn read<T: DeserializeOwned>(kv: &kv::Store, key: &str) -> Result<T> {
    let value = harnx_nats_common::recovery::read(|| kv.get(key))
        .await?
        .context("gate record missing")?;
    Ok(serde_json::from_slice(&value)?)
}

pub(super) async fn create<T: Serialize>(kv: &kv::Store, key: &str, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(
        bytes.len() <= MAX_RECORD_BYTES,
        "gate record exceeds 128 KiB"
    );
    kv.create(key, bytes.into()).await?;
    Ok(())
}
