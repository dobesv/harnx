use crate::{
    config::Config, nats_lease::NatsLeaseConfig, nats_session_index,
    nats_session_log::stream_name_for_session,
};
use anyhow::{Context, Result};
use async_nats::jetstream::{
    context::{DeleteStreamErrorKind, GetStreamErrorKind, KeyValueErrorKind},
    kv::Operation,
    ErrorCode,
};
use std::error::Error as _;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionDeleteResult {
    pub stream_deleted: bool,
    pub lease_deleted: bool,
}

impl SessionDeleteResult {
    pub fn removed_anything(&self) -> bool {
        self.stream_deleted || self.lease_deleted
    }
}

pub async fn delete_remote_session(
    config: &Config,
    cluster: &str,
    session_id: &str,
) -> Result<SessionDeleteResult> {
    let jetstream = config.nats_jetstream(cluster).await?;
    let lease_bucket = load_optional_lease_bucket(config, cluster).await?;
    let stream_deleted = delete_session_stream(&jetstream, session_id).await?;
    let lease_deleted = delete_session_lease(lease_bucket, session_id).await?;
    best_effort_delete_session_index_record(&jetstream, session_id).await;

    Ok(SessionDeleteResult {
        stream_deleted,
        lease_deleted,
    })
}

async fn best_effort_delete_session_index_record(
    jetstream: &async_nats::jetstream::Context,
    session_id: &str,
) {
    let store = match jetstream
        .get_key_value(nats_session_index::SESSION_INDEX_BUCKET)
        .await
    {
        Ok(store) => store,
        Err(error) if kv_bucket_missing(&error) => return,
        Err(error) => {
            log::warn!(
                "failed to open remote session index for cleanup: session_id={} err={error:#}",
                session_id
            );
            return;
        }
    };

    if let Err(error) = nats_session_index::delete_record(&store, session_id).await {
        log::warn!(
            "failed to delete remote session index record: session_id={} err={error:#}",
            session_id
        );
    }
}

async fn load_optional_lease_bucket(
    config: &Config,
    cluster: &str,
) -> Result<Option<async_nats::jetstream::kv::Store>> {
    match config.nats_kv_bucket(cluster, "harnx_leases").await {
        Ok(bucket) => Ok(Some(bucket)),
        Err(error) => {
            // Check if error chain contains a KeyValueError with bucket-not-found
            if error
                .chain()
                .find_map(|cause| {
                    cause.downcast_ref::<async_nats::jetstream::context::KeyValueError>()
                })
                .is_some_and(kv_bucket_missing)
            {
                Ok(None)
            } else {
                Err(error)
            }
        }
    }
}

async fn delete_session_stream(
    jetstream: &async_nats::jetstream::Context,
    session_id: &str,
) -> Result<bool> {
    let stream_name = stream_name_for_session(session_id);
    match jetstream.delete_stream(&stream_name).await {
        Ok(_) => Ok(true),
        Err(error) if delete_stream_missing(&error.kind()) => Ok(false),
        Err(error) => Err(error).with_context(|| {
            format!("Failed to delete session stream '{stream_name}' for '{session_id}'")
        }),
    }
}

async fn delete_session_lease(
    lease_bucket: Option<async_nats::jetstream::kv::Store>,
    session_id: &str,
) -> Result<bool> {
    let Some(lease_bucket) = lease_bucket else {
        return Ok(false);
    };
    let lease_key = NatsLeaseConfig::default().key_for_session(session_id);
    match lease_bucket.entry(lease_key.clone()).await {
        Ok(Some(entry)) if matches!(entry.operation, Operation::Put) => {
            lease_bucket
                .delete(&lease_key)
                .await
                .with_context(|| format!("Failed to delete session lease key '{lease_key}'"))?;
            Ok(true)
        }
        Ok(Some(_)) | Ok(None) => Ok(false),
        Err(error) => {
            Err(error).with_context(|| format!("Failed to inspect session lease key '{lease_key}'"))
        }
    }
}

fn get_stream_missing(kind: &GetStreamErrorKind) -> bool {
    match kind {
        GetStreamErrorKind::JetStream(error) => error.kind() == ErrorCode::STREAM_NOT_FOUND,
        _ => false,
    }
}

fn delete_stream_missing(kind: &DeleteStreamErrorKind) -> bool {
    match kind {
        DeleteStreamErrorKind::JetStream(error) => error.kind() == ErrorCode::STREAM_NOT_FOUND,
        _ => false,
    }
}

/// Check if an async_nats KeyValueError indicates a missing bucket (stream not found).
pub(crate) fn kv_bucket_missing(error: &async_nats::jetstream::context::KeyValueError) -> bool {
    match error.kind() {
        KeyValueErrorKind::GetBucket => error
            .source()
            .and_then(|source| {
                source.downcast_ref::<async_nats::jetstream::context::GetStreamError>()
            })
            .is_some_and(|stream_error| get_stream_missing(&stream_error.kind())),
        _ => false,
    }
}
