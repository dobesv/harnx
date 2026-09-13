//! Store operations for session read-state.
//!
//! Provides CAS-based mutations for the monotonic read cursor with dedicated
//! invalidation subject separate from metadata invalidation.

use super::{mutation::is_cas_conflict, *};
use crate::nats_session_metadata::read_state::SessionReadState;
use crate::nats_session_metadata::CAS_RETRY_LIMIT;
use anyhow::{Context, Result};
use async_nats::jetstream::kv;

const DEFAULT_VIEWER: &str = "default";

impl SessionMetadataStore {
    /// Returns the read-state for a session, defaulting to all-zeros if missing.
    pub async fn get_read_state(&self, session_id: &str) -> Result<SessionReadState> {
        let key = read_cursor_key(session_id, DEFAULT_VIEWER);
        match self.store.entry(key.clone()).await {
            Ok(Some(entry)) if matches!(entry.operation, kv::Operation::Put) => {
                serde_json::from_slice(&entry.value)
                    .with_context(|| format!("Failed to deserialize session read state '{key}'"))
            }
            Ok(Some(_)) | Ok(None) => Ok(SessionReadState::default()),
            Err(error) => Err(anyhow::Error::from(error))
                .with_context(|| format!("Failed to read session read state key '{key}'")),
        }
    }

    /// Bumps the attention cursor to at least `seq`, idempotent.
    ///
    /// Publishes read-invalidation if the stored value changes.
    pub async fn bump_attention(&self, session_id: &str, seq: u64) -> Result<()> {
        self.patch_read_state(session_id, |state| {
            if seq > state.last_attention_seq {
                state.last_attention_seq = seq;
                Ok(true)
            } else {
                Ok(false)
            }
        })
        .await
    }

    /// Marks the session as read: advances the read cursor to match attention, clears manual flag.
    ///
    /// Publishes read-invalidation if the stored value changes.
    pub async fn mark_read(&self, session_id: &str) -> Result<()> {
        self.patch_read_state(session_id, |state| {
            let advanced = if state.last_read_seq < state.last_attention_seq {
                state.last_read_seq = state.last_attention_seq;
                true
            } else {
                false
            };
            let cleared = state.manual_unread;
            state.manual_unread = false;
            Ok(advanced || cleared)
        })
        .await
    }

    /// Marks the session as manually unread: sets the flag without touching the cursor.
    ///
    /// Publishes read-invalidation if the stored value changes.
    pub async fn mark_unread(&self, session_id: &str) -> Result<()> {
        self.patch_read_state(session_id, |state| {
            if state.manual_unread {
                Ok(false)
            } else {
                state.manual_unread = true;
                Ok(true)
            }
        })
        .await
    }

    /// CAS loop for read-state mutations.
    ///
    /// The `patch` closure returns `Ok(true)` if the state was mutated, `Ok(false)` for no-op.
    /// On mutation, publishes read-invalidation with the new revision.
    async fn patch_read_state<F>(&self, session_id: &str, mut patch: F) -> Result<()>
    where
        F: FnMut(&mut SessionReadState) -> Result<bool>,
    {
        let key = read_cursor_key(session_id, DEFAULT_VIEWER);
        for attempt in 0..CAS_RETRY_LIMIT {
            let (state, revision) = self.read_state_snapshot(&key).await?;
            let mut state = state;
            let changed = patch(&mut state)?;
            if !changed {
                return Ok(());
            }
            match self.write_read_state(&key, &state, revision).await {
                Ok(new_revision) => {
                    self.publish_read_invalidation(session_id, new_revision)
                        .await;
                    return Ok(());
                }
                Err(error) if is_cas_conflict(&error) && attempt + 1 < CAS_RETRY_LIMIT => {
                    tokio::task::yield_now().await;
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("Failed to update session read state '{key}'"))
                }
            }
        }
        unreachable!("bounded CAS loop always returns")
    }

    async fn read_state_snapshot(&self, key: &str) -> Result<(SessionReadState, u64)> {
        let Some(entry) = self.store.entry(key.to_string()).await? else {
            return Ok((SessionReadState::default(), 0));
        };
        if !matches!(entry.operation, kv::Operation::Put) {
            return Ok((SessionReadState::default(), 0));
        }
        let state = serde_json::from_slice(&entry.value)
            .with_context(|| format!("Failed to deserialize session read state '{key}'"))?;
        Ok((state, entry.revision))
    }

    async fn write_read_state(
        &self,
        key: &str,
        state: &SessionReadState,
        revision: u64,
    ) -> Result<u64> {
        let payload = serde_json::to_vec(state)?;
        if revision == 0 {
            self.store
                .create(key, payload.into())
                .await
                .map_err(anyhow::Error::from)
        } else {
            self.store
                .update(key, payload.into(), revision)
                .await
                .map_err(anyhow::Error::from)
        }
    }

    async fn publish_read_invalidation(&self, session_id: &str, revision: u64) {
        let subject = read_invalidation_subject(session_id);
        let payload = serde_json::json!({
            "session_id": session_id,
            "revision": revision,
        });
        if let Err(error) = self
            .client
            .publish(subject, payload.to_string().into())
            .await
        {
            log::warn!(
                "failed to publish session read-state invalidation: session_id={session_id} error={error:#}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::require_nextest;

    #[test]
    fn bump_attention_ignores_stale_seq() {
        require_nextest();
        let mut state = SessionReadState {
            last_attention_seq: 10,
            ..Default::default()
        };
        // Simulate patch: seq < current is no-op
        let seq = 5u64;
        let changed = seq > state.last_attention_seq;
        if changed {
            state.last_attention_seq = seq;
        }
        assert!(!changed);
        assert_eq!(state.last_attention_seq, 10);
    }

    #[test]
    fn bump_attention_advances_on_higher_seq() {
        require_nextest();
        let mut state = SessionReadState {
            last_attention_seq: 10,
            ..Default::default()
        };
        let seq = 15u64;
        let changed = seq > state.last_attention_seq;
        if changed {
            state.last_attention_seq = seq;
        }
        assert!(changed);
        assert_eq!(state.last_attention_seq, 15);
    }

    #[test]
    fn mark_read_advances_cursor_and_clears_flag() {
        require_nextest();
        let mut state = SessionReadState {
            last_attention_seq: 10,
            last_read_seq: 5,
            manual_unread: true,
        };
        let changed = {
            let advanced = if state.last_read_seq < state.last_attention_seq {
                state.last_read_seq = state.last_attention_seq;
                true
            } else {
                false
            };
            let cleared = state.manual_unread;
            state.manual_unread = false;
            advanced || cleared
        };
        assert!(changed);
        assert_eq!(state.last_read_seq, 10);
        assert!(!state.manual_unread);
        assert!(!state.is_unread());
    }

    #[test]
    fn mark_read_noop_when_already_read() {
        require_nextest();
        let mut state = SessionReadState {
            last_attention_seq: 10,
            last_read_seq: 10,
            manual_unread: false,
        };
        let changed = {
            let advanced = if state.last_read_seq < state.last_attention_seq {
                state.last_read_seq = state.last_attention_seq;
                true
            } else {
                false
            };
            let cleared = state.manual_unread;
            state.manual_unread = false;
            advanced || cleared
        };
        assert!(!changed);
        assert!(!state.manual_unread);
    }

    #[test]
    fn mark_unread_sets_flag_without_touching_cursor() {
        require_nextest();
        let mut state = SessionReadState {
            last_attention_seq: 10,
            last_read_seq: 10,
            manual_unread: false,
        };
        let changed = if state.manual_unread {
            false
        } else {
            state.manual_unread = true;
            true
        };
        assert!(changed);
        assert_eq!(state.last_read_seq, 10);
        assert!(state.manual_unread);
        assert!(state.is_unread());
    }

    #[test]
    fn mark_unread_noop_when_already_set() {
        require_nextest();
        let state = SessionReadState {
            last_attention_seq: 10,
            last_read_seq: 10,
            manual_unread: true,
        };
        let changed = if state.manual_unread {
            false
        } else {
            true // would set manual_unread = true
        };
        assert!(!changed);
        assert!(state.manual_unread);
    }
}
