//! Find `SESSION_` JetStream streams that have no session metadata, for
//! `harnx list orphan-streams`.
//!
//! Older session-creation paths created the stream (`nats_session_log::ensure_stream`)
//! before writing canonical metadata (`sessions/<storage_key>/meta` in
//! `harnx_sessions`), and failed metadata writes left streams behind. A
//! metadata bucket that is lost or recreated strands every stream it described
//! the same way. The metadata GC enumerates KV metadata, so it can't see them;
//! [`crate::session_stream_gc`] deletes them once they have gone unwritten for
//! the retention window.
//!
//! The stream name is `SESSION_<sha256(storage_key)>` and cannot be reversed,
//! so streams are mapped back to their storage key through the stream subject
//! (`sessions.<storage_key>.log`) and verified against
//! [`crate::nats_session_log::stream_name_for_session`]. Streams that cannot be
//! mapped are reported and never touched.

use crate::config::Config;
use crate::nats_session_log::{stream_name_for_session, STREAM_NAME_PREFIX};
use crate::session_stream_gc::StreamGcContext;
use anyhow::Result;
use async_nats::jetstream::stream::{Config as StreamConfig, Info as StreamInfo};
use futures_util::TryStreamExt;

/// One `SESSION_` stream with no session metadata behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanStreamReport {
    pub stream_name: String,
    /// Storage key recovered from the stream subject and verified against the
    /// stream name.
    pub storage_key: String,
    pub created_unix_secs: u64,
    pub messages: u64,
    pub bytes: u64,
    pub last_message_unix_secs: Option<u64>,
    pub session_lease_present: bool,
    pub a2a_lease_present: bool,
    pub a2a_unresolved_tasks: bool,
}

/// A `SESSION_` stream whose subject could not be mapped to a storage key.
/// Reported for manual review; never deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnmappedStreamReport {
    pub stream_name: String,
    pub subjects: Vec<String>,
    pub reason: String,
}

/// All `SESSION_` streams on one cluster, split into mapped orphans (no
/// metadata) and unmappable streams.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OrphanStreamInventory {
    pub orphans: Vec<OrphanStreamReport>,
    pub unmapped: Vec<UnmappedStreamReport>,
}

/// List all `SESSION_` streams on `cluster` that have no session metadata.
///
/// Read-only. Metadata is read from the stream leader like every other
/// metadata read, so a follower that has not applied a just-written metadata
/// record cannot produce a false orphan.
pub async fn list_orphan_session_streams(
    config: &Config,
    cluster: &str,
) -> Result<OrphanStreamInventory> {
    let ctx = StreamGcContext::connect(config, cluster).await?;
    let mut inventory = OrphanStreamInventory::default();
    let mut streams = ctx.jetstream.streams();
    while let Some(info) = streams.try_next().await? {
        if is_session_stream(&info.config) {
            inventory_stream(&ctx, &mut inventory, &info).await?;
        }
    }
    Ok(inventory)
}

async fn inventory_stream(
    ctx: &StreamGcContext,
    inventory: &mut OrphanStreamInventory,
    info: &StreamInfo,
) -> Result<()> {
    match storage_key_from_stream(&info.config) {
        Ok(storage_key) => {
            if !ctx.metadata_present(&storage_key).await? {
                inventory
                    .orphans
                    .push(report_orphan(ctx, info, &storage_key).await?);
            }
        }
        Err(reason) => inventory.unmapped.push(UnmappedStreamReport {
            stream_name: info.config.name.clone(),
            subjects: info.config.subjects.clone(),
            reason,
        }),
    }
    Ok(())
}

async fn report_orphan(
    ctx: &StreamGcContext,
    info: &StreamInfo,
    storage_key: &str,
) -> Result<OrphanStreamReport> {
    let leases = ctx.lease_store.as_ref();
    Ok(OrphanStreamReport {
        stream_name: info.config.name.clone(),
        storage_key: storage_key.to_string(),
        created_unix_secs: unix_secs(info.created.unix_timestamp()),
        messages: info.state.messages,
        bytes: info.state.bytes,
        last_message_unix_secs: (info.state.messages > 0)
            .then(|| unix_secs(info.state.last_timestamp.unix_timestamp())),
        session_lease_present: crate::remote_session_cleanup::session_lease_present(
            leases,
            storage_key,
        )
        .await?,
        a2a_lease_present: crate::remote_session_cleanup::a2a_lease_present(leases, storage_key)
            .await?,
        a2a_unresolved_tasks: crate::nats_admin::a2a::unresolved(&ctx.metadata_store, storage_key)
            .await?,
    })
}

pub(crate) fn is_session_stream(config: &StreamConfig) -> bool {
    config.name.starts_with(STREAM_NAME_PREFIX)
}

/// Map a `SESSION_` stream back to its storage key through the stream subject
/// (`sessions.<storage_key>.log`, or the wider `sessions.<storage_key>.>`
/// some deployments create), then verify the stream name is the key's hash.
/// `Err` carries the reason the stream cannot be mapped; unmappable streams
/// are reported and never deleted.
pub(crate) fn storage_key_from_stream(
    config: &StreamConfig,
) -> std::result::Result<String, String> {
    for subject in &config.subjects {
        let Some(key) = storage_key_from_subject(subject) else {
            continue;
        };
        if stream_name_for_session(&key) == config.name {
            return Ok(key);
        }
        return Err(format!(
            "subject '{subject}' maps to key '{key}' but the stream name is not its hash"
        ));
    }
    Err(match config.subjects.as_slice() {
        [] => "the stream has no subjects".to_string(),
        subjects => format!(
            "no subject of the form sessions.<storage_key>.log or sessions.<storage_key>.> found \
             (subjects: {subjects:?})"
        ),
    })
}

/// `sessions.<storage_key>.log` / `sessions.<storage_key>.>` ->
/// `<storage_key>`. Keys are SHA-256 hex, so a nested dot means the subject
/// belongs to something else.
fn storage_key_from_subject(subject: &str) -> Option<String> {
    let key = subject.strip_prefix("sessions.")?;
    let key = key
        .strip_suffix(".log")
        .or_else(|| key.strip_suffix(".>"))
        .or_else(|| key.strip_suffix(".*"))?;
    if key.is_empty() || key.contains('.') {
        return None;
    }
    Some(key.to_string())
}

pub(crate) fn unix_secs(timestamp: i64) -> u64 {
    timestamp.max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(name: String, subjects: Vec<String>) -> StreamConfig {
        StreamConfig {
            name,
            subjects,
            ..Default::default()
        }
    }

    #[test]
    fn session_stream_prefix_matches_stream_name_for_session() {
        let name = stream_name_for_session("some-storage-key");
        assert!(name.starts_with(STREAM_NAME_PREFIX));
        assert!(is_session_stream(&stream(name, Vec::new())));
        assert!(!is_session_stream(&stream(
            "KV_harnx_sessions".into(),
            Vec::new()
        )));
    }

    #[test]
    fn storage_key_round_trips_through_subject() {
        let key = harnx_core::session_identity::session_key(Some("athena"), "orphan-map");
        let subject = crate::nats_session_log::subject_for_session(&key);
        assert_eq!(
            storage_key_from_subject(&subject).as_deref(),
            Some(key.as_str())
        );
        assert_eq!(
            storage_key_from_subject(&format!("sessions.{key}.>")).as_deref(),
            Some(key.as_str())
        );
    }

    #[test]
    fn storage_key_rejects_unmappable_subjects() {
        assert_eq!(storage_key_from_subject("sessions..log"), None);
        assert_eq!(storage_key_from_subject("sessions.a.b.log"), None);
        assert_eq!(storage_key_from_subject("sessions.abc"), None);
        assert_eq!(storage_key_from_subject("other.subject.log"), None);
        assert_eq!(storage_key_from_subject("sessions."), None);
    }

    #[test]
    fn storage_key_from_stream_requires_name_hash_match() {
        let key = harnx_core::session_identity::session_key(Some("athena"), "hash-match");
        let subject = crate::nats_session_log::subject_for_session(&key);
        let name = stream_name_for_session(&key);
        let matched = storage_key_from_stream(&stream(name.clone(), vec![subject.clone()]));
        assert_eq!(matched.expect("mapped"), key);

        // A stream whose subject parses but whose name is not the key's hash
        // must be reported, not mapped.
        let mismatched = storage_key_from_stream(&stream(
            stream_name_for_session(&format!("{key}-other")),
            vec![subject],
        ));
        assert!(mismatched.is_err());

        assert!(storage_key_from_stream(&stream(name.clone(), Vec::new())).is_err());
        assert!(storage_key_from_stream(&stream(name, vec!["other.subject".into()])).is_err());
    }
}
