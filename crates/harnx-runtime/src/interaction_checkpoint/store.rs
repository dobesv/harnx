//! Shared, no-TTL private storage. Large values are committed by manifest last.
use super::{records, round_start_key, RoundStart, ROUND_START_BUCKET};
use anyhow::{ensure, Context, Result};
use async_nats::jetstream::{self, context::GetStreamErrorKind, kv, ErrorCode};
use harnx_nats_common::leader_reads;
use serde::{de::DeserializeOwned, Deserialize, Serialize};

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;

/// Includes discarded dispatch results and all attempts, not just the last stash.
pub const MAX_CALL_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_VALUE_BYTES: usize = 8 * 1024 * 1024;
const CHUNK_BYTES: usize = 256 * 1024;
const BROKER_HEADER_ALLOWANCE: usize = 4096;
const RESERVATION_CAS_ATTEMPTS: usize = 32;

#[derive(Debug, PartialEq, Eq)]
pub enum ReadCheckpoint<T> {
    Found(T),
    Missing(String),
}

// qual:allow(srp, god_struct) reason: "The bucket and its JetStream context form one private store handle, including leader reads and key listings."
#[derive(Clone)]
pub struct CheckpointStore {
    pub(super) kv: kv::Store,
    pub(super) js: jetstream::Context,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Blob {
    version: u32,
    bytes: usize,
    sha256: String,
    chunks: usize,
}

fn chunk_key(key: &str, index: usize) -> String {
    format!("{key}.chunks.{index}")
}

fn validate_manifest_blob(manifest: &[u8]) -> Result<Blob, String> {
    let blob: Blob =
        serde_json::from_slice(manifest).map_err(|e| format!("invalid manifest JSON: {e}"))?;
    if blob.version != 1
        || blob.bytes > MAX_VALUE_BYTES
        || blob.chunks != blob.bytes.div_ceil(CHUNK_BYTES)
    {
        return Err("invalid interaction checkpoint manifest header".to_string());
    }
    Ok(blob)
}

fn validate_blob_checksum(bytes: &[u8], blob: &Blob) -> Result<(), String> {
    let hash = match std::str::from_utf8(bytes) {
        Ok(s) => harnx_core::crypto::sha256(s),
        Err(_) => return Err("non-utf8 bytes".to_string()),
    };
    if bytes.len() != blob.bytes || hash != blob.sha256 {
        return Err("checksum mismatch".to_string());
    }
    Ok(())
}

impl CheckpointStore {
    pub async fn ensure(js: &jetstream::Context, replicas: usize) -> Result<Self> {
        let kv = match js
            .create_key_value(kv::Config {
                bucket: ROUND_START_BUCKET.into(),
                description: "Private interaction checkpoints and write-ahead records".into(),
                num_replicas: replicas,
                history: 1,
                max_value_size: (CHUNK_BYTES + BROKER_HEADER_ALLOWANCE) as i32,
                ..Default::default()
            })
            .await
        {
            Ok(store) => store,
            Err(_) => {
                harnx_nats_common::registry::reconcile_bucket(
                    js,
                    ROUND_START_BUCKET,
                    replicas,
                    |config| {
                        config.max_age = std::time::Duration::ZERO;
                        config.max_message_size = (CHUNK_BYTES + BROKER_HEADER_ALLOWANCE) as i32;
                    },
                )
                .await
                .context("open interaction checkpoint store")?;
                js.get_key_value(ROUND_START_BUCKET).await?
            }
        };
        Ok(Self { kv, js: js.clone() })
    }

    pub async fn start_round(&self, start: &RoundStart) -> Result<()> {
        ensure!(
            start.version == 1,
            "unsupported interaction round-start version"
        );
        let key = round_start_key(&start.session_storage_key, start.tool_round_seq);
        // Round ownership predates every attempt. Keep the original fence on resume.
        if let Some(bytes) = self.get(&key).await? {
            let previous: RoundStart = serde_json::from_slice(&bytes)?;
            ensure!(
                previous.version == 1
                    && previous.session_storage_key == start.session_storage_key
                    && previous.tool_round_seq == start.tool_round_seq,
                "interaction round-start identity mismatch"
            );
            return Ok(());
        }
        self.kv
            .create(key, serde_json::to_vec(start)?.into())
            .await?;
        Ok(())
    }

    pub async fn get(&self, key: &str) -> Result<Option<bytes::Bytes>> {
        if crate::fault_injection::is_store_error_simulated(key) {
            return Err(anyhow::anyhow!("simulated checkpoint store unreachable"));
        }
        leader_reads::retry_transient(|| leader_reads::get(&self.kv, key)).await
    }

    /// All records are immutable and scoped under the original call. Reservations
    /// survive interrupted uploads so a crash cannot reset the retained-byte bound.
    pub async fn put<T: Serialize>(
        &self,
        call_prefix: &str,
        suffix: &str,
        value: &T,
    ) -> Result<String> {
        if crate::fault_injection::is_store_error_simulated(call_prefix) {
            return Err(anyhow::anyhow!("simulated checkpoint store unreachable"));
        }
        let bytes = serde_json::to_vec(value)?;
        ensure!(
            bytes.len() <= MAX_VALUE_BYTES,
            "interaction checkpoint value exceeds 8 MiB"
        );
        self.reserve(call_prefix, bytes.len() as u64).await?;
        let key = format!("{call_prefix}{suffix}");
        let mut committed_chunks = 0;
        let write = async {
            for (index, chunk) in bytes.chunks(CHUNK_BYTES).enumerate() {
                self.kv
                    .create(chunk_key(&key, index), chunk.to_vec().into())
                    .await?;
                committed_chunks = index + 1;
            }
            let blob = Blob {
                version: 1,
                bytes: bytes.len(),
                sha256: harnx_core::crypto::sha256(std::str::from_utf8(&bytes)?),
                chunks: bytes.len().div_ceil(CHUNK_BYTES),
            };
            self.kv
                .create(key.clone(), serde_json::to_vec(&blob)?.into())
                .await?;
            Ok::<(), anyhow::Error>(())
        }
        .await;
        if let Err(error) = write {
            // The reference never escaped, so reconcile instead of leaving the
            // failed attempt counted against the call's retained-byte bound. A
            // crash skips this by construction; if cleanup fails here the
            // reservation stays, keeping the bound fail-closed either way.
            if let Err(cleanup) = self
                .reconcile_failed_put(call_prefix, &key, committed_chunks, bytes.len() as u64)
                .await
            {
                tracing::debug!(
                    error = %cleanup,
                    "failed interaction checkpoint put kept its retained-byte reservation"
                );
            }
            return Err(error);
        }
        Ok(key)
    }

    /// Remove the chunks a failed [`put`](Self::put) already committed and hand
    /// the reservation back. Only indices the failed call itself created are
    /// purged: `kv::Store::create` fails on an existing key, so every lower
    /// index was written by this call. The purge is physical: a `kv::Store`
    /// purge marker would keep the chunk's revision alive and block a retry of
    /// the same record key forever.
    async fn reconcile_failed_put(
        &self,
        call_prefix: &str,
        key: &str,
        committed_chunks: usize,
        reserved: u64,
    ) -> Result<()> {
        for index in 0..committed_chunks {
            let subject = format!("{}{}", self.kv.prefix, chunk_key(key, index));
            self.kv.stream.purge().filter(subject).await?;
        }
        self.release(call_prefix, reserved).await
    }

    pub async fn read<T: DeserializeOwned>(&self, call_prefix: &str, key: &str) -> Result<T> {
        ensure!(
            key.starts_with(call_prefix) && !key[call_prefix.len()..].is_empty(),
            "interaction checkpoint reference outside owning call"
        );
        let manifest = self
            .get(key)
            .await?
            .context("missing interaction checkpoint")?;
        let blob: Blob = serde_json::from_slice(&manifest)?;
        ensure!(
            blob.version == 1
                && blob.bytes <= MAX_VALUE_BYTES
                && blob.chunks == blob.bytes.div_ceil(CHUNK_BYTES),
            "invalid interaction checkpoint manifest"
        );
        let mut bytes = Vec::with_capacity(blob.bytes);
        for index in 0..blob.chunks {
            let chunk = self
                .get(&chunk_key(key, index))
                .await?
                .context("missing interaction checkpoint chunk")?;
            ensure!(
                chunk.len() <= CHUNK_BYTES,
                "oversized interaction checkpoint chunk"
            );
            bytes.extend_from_slice(&chunk);
        }
        ensure!(
            bytes.len() == blob.bytes
                && harnx_core::crypto::sha256(std::str::from_utf8(&bytes)?) == blob.sha256,
            "interaction checkpoint checksum mismatch"
        );
        serde_json::from_slice(&bytes).context("decode private interaction checkpoint")
    }

    async fn reserve(&self, prefix: &str, bytes: u64) -> Result<()> {
        let key = format!("{prefix}retained_bytes");
        for _ in 0..RESERVATION_CAS_ATTEMPTS {
            let previous =
                leader_reads::retry_transient(|| leader_reads::entry(&self.kv, &key)).await?;
            let used: u64 = previous
                .as_ref()
                .filter(|e| e.operation == kv::Operation::Put)
                .map(|e| serde_json::from_slice(&e.value))
                .transpose()?
                .unwrap_or(0);
            let total = used
                .checked_add(bytes)
                .context("interaction retained-byte overflow")?;
            ensure!(
                total <= MAX_CALL_BYTES,
                "interaction retained data exceeds 64 MiB per call"
            );
            let revision = previous.as_ref().map_or(0, |e| e.revision);
            match self
                .kv
                .update(&key, serde_json::to_vec(&total)?.into(), revision)
                .await
            {
                Ok(_) => return Ok(()),
                Err(error) if error.kind() == kv::UpdateErrorKind::WrongLastRevision => continue,
                Err(error) => return Err(error.into()),
            }
        }
        anyhow::bail!("interaction retained-byte reservation contention")
    }

    /// Hand a failed put's reservation back. The key may already be gone,
    /// purged by round cleanup or session deletion, in which case nothing is
    /// retained and the release is a no-op.
    async fn release(&self, prefix: &str, bytes: u64) -> Result<()> {
        if bytes == 0 {
            return Ok(());
        }
        let key = format!("{prefix}retained_bytes");
        for _ in 0..RESERVATION_CAS_ATTEMPTS {
            let previous =
                leader_reads::retry_transient(|| leader_reads::entry(&self.kv, &key)).await?;
            let Some(entry) = previous.filter(|entry| entry.operation == kv::Operation::Put) else {
                return Ok(());
            };
            let used: u64 = serde_json::from_slice(&entry.value)?;
            let total = used.saturating_sub(bytes);
            match self
                .kv
                .update(&key, serde_json::to_vec(&total)?.into(), entry.revision)
                .await
            {
                Ok(_) => return Ok(()),
                Err(error) if error.kind() == kv::UpdateErrorKind::WrongLastRevision => continue,
                Err(error) => return Err(error.into()),
            }
        }
        anyhow::bail!("interaction retained-byte release contention")
    }

    pub async fn call_has_dispatched(&self, session: &str, round: u64, call: &str) -> Result<bool> {
        let prefix = records::call_prefix(session, round, call);
        Ok(self
            .keys(&format!("{prefix}attempts.*.dispatches.*.intent"))
            .await?
            .into_iter()
            .any(|key| !key.contains(".chunks")))
    }

    pub async fn open_existing(js: &jetstream::Context) -> Result<Option<Self>> {
        match js.get_stream(format!("KV_{ROUND_START_BUCKET}")).await {
            Ok(_) => {
                let kv = js.get_key_value(ROUND_START_BUCKET).await?;
                Ok(Some(Self { kv, js: js.clone() }))
            }
            Err(error)
                if matches!(error.kind(), GetStreamErrorKind::JetStream(ref error)
                    if error.kind() == ErrorCode::STREAM_NOT_FOUND) =>
            {
                Ok(None)
            }
            Err(error) => Err(error).context("open interaction checkpoint bucket"),
        }
    }

    pub async fn read_optional<T: DeserializeOwned>(
        &self,
        call_prefix: &str,
        key: &str,
    ) -> Result<ReadCheckpoint<T>> {
        if !key.starts_with(call_prefix) || key[call_prefix.len()..].is_empty() {
            return Ok(ReadCheckpoint::Missing(
                "interaction checkpoint reference outside owning call".to_string(),
            ));
        }
        let manifest = match self.get(key).await? {
            Some(bytes) => bytes,
            None => {
                return Ok(ReadCheckpoint::Missing(
                    "missing interaction checkpoint manifest".to_string(),
                ));
            }
        };
        let blob = match validate_manifest_blob(&manifest) {
            Ok(b) => b,
            Err(reason) => return Ok(ReadCheckpoint::Missing(reason)),
        };
        let mut bytes = Vec::with_capacity(blob.bytes);
        for index in 0..blob.chunks {
            match self.get(&chunk_key(key, index)).await? {
                Some(chunk) => {
                    if chunk.len() > CHUNK_BYTES {
                        return Ok(ReadCheckpoint::Missing("oversized chunk".to_string()));
                    }
                    bytes.extend_from_slice(&chunk);
                }
                None => {
                    return Ok(ReadCheckpoint::Missing(format!("missing chunk {index}")));
                }
            }
        }
        if let Err(reason) = validate_blob_checksum(&bytes, &blob) {
            return Ok(ReadCheckpoint::Missing(reason));
        }
        match serde_json::from_slice(&bytes) {
            Ok(val) => Ok(ReadCheckpoint::Found(val)),
            Err(e) => Ok(ReadCheckpoint::Missing(format!("decode error: {e}"))),
        }
    }

    pub async fn read_ordered_attempts(
        &self,
        session_storage_key: &str,
        round: u64,
        call_id: &str,
    ) -> Result<Result<Vec<records::AttemptStart>, String>> {
        let call_prefix = records::call_prefix(session_storage_key, round, call_id);
        let pattern = format!("{call_prefix}attempts.*.start");
        let keys = self.keys(&pattern).await?;
        let manifest_keys: Vec<String> = keys
            .into_iter()
            .filter(|k| !k.contains(".chunks"))
            .collect();
        if manifest_keys.is_empty() {
            return Ok(Ok(Vec::new()));
        }
        let mut attempts = Vec::with_capacity(manifest_keys.len());
        for key in manifest_keys {
            let manifest_id = match key
                .strip_prefix(&call_prefix)
                .and_then(|k| k.strip_prefix("attempts."))
                .and_then(|k| k.strip_suffix(".start"))
            {
                Some(id) if !id.is_empty() => id,
                _ => return Ok(Err("invalid attempt manifest key layout".to_string())),
            };
            let entry =
                leader_reads::retry_transient(|| leader_reads::entry(&self.kv, &key)).await?;
            let Some(e) = entry else {
                return Ok(Err("missing attempt manifest entry".to_string()));
            };
            match self
                .read_optional::<records::AttemptStart>(&call_prefix, &key)
                .await?
            {
                ReadCheckpoint::Found(attempt) => {
                    if attempt.version != 1
                        || attempt.session_storage_key != session_storage_key
                        || attempt.tool_round_seq != round
                        || attempt.tool_call_id != call_id
                        || attempt.attempt_id != manifest_id
                    {
                        return Ok(Err("attempt identity mismatch".to_string()));
                    }
                    attempts.push((e.revision, attempt));
                }
                ReadCheckpoint::Missing(reason) => {
                    return Ok(Err(format!("missing attempt data: {reason}")));
                }
            }
        }
        attempts.sort_by_key(|(rev, _)| *rev);
        Ok(Ok(attempts.into_iter().map(|(_, att)| att).collect()))
    }
}
