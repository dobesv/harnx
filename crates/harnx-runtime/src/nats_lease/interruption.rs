//! Accepted interruption revokes only the original lease, never a replacement.
use super::{is_create_conflict, LeaseRecord};
use anyhow::{Context, Result};
use async_nats::jetstream::{self, kv};
use harnx_execution_control::{ExecutionStore, OperationRef};
use std::time::Duration;

pub(super) struct LeaseCreate<'a> {
    pub key: &'a str,
    pub session_id: &'a str,
    pub record: &'a LeaseRecord,
    pub ttl: Duration,
}

pub(super) async fn create_lease(
    jetstream: &jetstream::Context,
    bucket: &kv::Store,
    request: LeaseCreate<'_>,
) -> Result<Option<u64>> {
    let payload = serde_json::to_vec(request.record).context("Failed to serialize lease record")?;
    if let Some(revision) = try_create_lease(bucket, &request, &payload).await? {
        return Ok(Some(revision));
    }
    if request.record.execution_id.is_none() {
        return Ok(None);
    }
    if !revoke_interrupted_lease(jetstream, bucket, &request).await? {
        return Ok(None);
    }
    // Retry once after revocation. A new conflict belongs to another owner.
    try_create_lease(bucket, &request, &payload).await
}

async fn try_create_lease(
    bucket: &kv::Store,
    request: &LeaseCreate<'_>,
    payload: &[u8],
) -> Result<Option<u64>> {
    match bucket
        .create_with_ttl(request.key, payload.to_vec().into(), request.ttl)
        .await
    {
        Ok(revision) => Ok(Some(revision)),
        Err(error) if is_create_conflict(&error) => Ok(None),
        Err(error) => Err(error).context("Failed to acquire NATS lease"),
    }
}

async fn revoke_interrupted_lease(
    jetstream: &jetstream::Context,
    bucket: &kv::Store,
    request: &LeaseCreate<'_>,
) -> Result<bool> {
    let Some(entry) = bucket.entry(request.key).await? else {
        return Ok(true);
    };
    if entry.operation != kv::Operation::Put {
        return Ok(true);
    }
    let record: LeaseRecord = serde_json::from_slice(&entry.value)?;
    let Some(execution_id) = record.execution_id else {
        return Ok(false);
    };
    let reference = OperationRef::new(request.session_id, execution_id);
    let store = ExecutionStore::from_store(
        jetstream
            .get_key_value(harnx_execution_control::BUCKET)
            .await?,
    );
    if !execution_is_revoked(&store, &reference).await? {
        return Ok(false);
    }
    // Stop evidence is monotonic. A renewal or G2 acquisition after this read
    // changes the lease revision and defeats deletion. New work still needs gate CAS.
    match bucket
        .delete_expect_revision(request.key, Some(entry.revision))
        .await
    {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == kv::UpdateErrorKind::WrongLastRevision => Ok(false),
        Err(error) => Err(error.into()),
    }
}

async fn execution_is_revoked(store: &ExecutionStore, reference: &OperationRef) -> Result<bool> {
    if store.accepted_stop(reference).await?.is_some() {
        return Ok(true);
    }
    // The registration marker and pre-gate cancellation share the physical CAS.
    // A cancellation with no marker permanently prevents this owner from activating.
    Ok(store.get(reference).await?.is_some_and(|operation| {
        operation.gate_registration.is_none() && operation.cancellation.is_some()
    }))
}
