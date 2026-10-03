//! Activity touch for session lifecycle.
//!
//! Rate-limited in-process to ≤1 write per owner per ~hour.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use async_nats::jetstream::{self, kv};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

/// KV bucket for session metadata (same as harnx_sessions).
const SESSION_METADATA_BUCKET: &str = "harnx_sessions";

/// Minimum time between activity touches for the same owner.
const MIN_TOUCH_INTERVAL_SECS: u64 = 3600; // 1 hour

/// In-process rate limiter for activity touches.
static ACTIVITY_TIMESTAMPS: std::sync::OnceLock<Arc<RwLock<HashMap<String, Instant>>>> =
    std::sync::OnceLock::new();

/// Session activity record (mirrors harnx-runtime's SessionActivity).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionActivity {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_activation_at: Option<DateTime<Utc>>,
    pub last_activity_at: DateTime<Utc>,
}

/// Result of an activity touch.
pub struct ActivityGuard {
    wrote: bool,
}

impl ActivityGuard {
    /// Check if a touch was performed.
    pub fn wrote(&self) -> bool {
        self.wrote
    }
}

fn activity_timestamps() -> &'static RwLock<HashMap<String, Instant>> {
    ACTIVITY_TIMESTAMPS
        .get_or_init(|| Arc::new(RwLock::new(HashMap::new())))
        .as_ref()
}

async fn should_skip_touch(owner: &str) -> bool {
    let elapsed = activity_timestamps()
        .read()
        .await
        .get(owner)
        .map(Instant::elapsed);
    match elapsed {
        Some(elapsed) if elapsed.as_secs() < MIN_TOUCH_INTERVAL_SECS => {
            log::debug!(
                "skipping activity touch for {} (last touched {}s ago, min {}s)",
                owner,
                elapsed.as_secs(),
                MIN_TOUCH_INTERVAL_SECS
            );
            true
        }
        _ => false,
    }
}

async fn get_or_create_metadata_bucket(jetstream: &jetstream::Context) -> Result<kv::Store> {
    match jetstream.get_key_value(SESSION_METADATA_BUCKET).await {
        Ok(store) => Ok(store),
        Err(error) if error.kind() == jetstream::context::KeyValueErrorKind::GetBucket => jetstream
            .create_key_value(kv::Config {
                bucket: SESSION_METADATA_BUCKET.to_string(),
                storage: jetstream::stream::StorageType::File,
                ..Default::default()
            })
            .await
            .context("failed to create session metadata bucket"),
        Err(error) => Err(error).context("failed to get session metadata bucket"),
    }
}

async fn update_session_activity(store: &kv::Store, key: &str, now: DateTime<Utc>) -> Result<()> {
    // The leader has the activity a new session's creator wrote a moment ago,
    // which a follower may not. Missing it, or failing to read it, would stamp
    // a first activation on a session that hasn't run yet.
    let current = harnx_nats_common::leader_reads::entry(store, key)
        .await
        .context("read session activity")?;
    let activity = match current {
        Some(entry) if matches!(entry.operation, kv::Operation::Put) => {
            let previous: SessionActivity = serde_json::from_slice(&entry.value)
                .context("failed to deserialize session activity")?;
            SessionActivity {
                first_activation_at: previous.first_activation_at,
                last_activity_at: now,
            }
        }
        _ => SessionActivity {
            first_activation_at: Some(now),
            last_activity_at: now,
        },
    };
    let payload = serde_json::to_vec(&activity).context("serialize session activity")?;
    store
        .put(key, payload.into())
        .await
        .with_context(|| format!("write session activity key '{key}'"))?;
    Ok(())
}

/// Touch the activity for an owner.
///
/// This is rate-limited in-process to ≤1 write per owner per ~hour.
///
/// # Arguments
///
/// * `jetstream` - JetStream context
/// * `owner` - The session owner key (64-char hex)
///
/// # Returns
///
/// Returns Ok(ActivityGuard). Check `guard.wrote()` to see if a write was performed.
pub async fn touch_activity(jetstream: &jetstream::Context, owner: &str) -> Result<ActivityGuard> {
    if should_skip_touch(owner).await {
        return Ok(ActivityGuard { wrote: false });
    }

    let store = get_or_create_metadata_bucket(jetstream).await?;
    let key = format!("sessions/{owner}/activity");
    update_session_activity(&store, &key, Utc::now()).await?;
    activity_timestamps()
        .write()
        .await
        .insert(owner.to_string(), Instant::now());

    Ok(ActivityGuard { wrote: true })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_serialization() {
        let activity = SessionActivity {
            first_activation_at: Some(Utc::now()),
            last_activity_at: Utc::now(),
        };
        let json = serde_json::to_string(&activity).unwrap();
        let back: SessionActivity = serde_json::from_str(&json).unwrap();
        assert_eq!(activity, back);
    }
}
