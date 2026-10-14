use super::status::ensure_waitable;
use super::{assess, SandboxManager, SandboxRecord, WaitContext};
use anyhow::{Context, Result};
use chrono::Utc;
use std::time::Duration;
use tokio::time::Instant;

/// Keep slow activations safe from the idle watcher without bumping on every poll (#2405).
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(60);
/// Bound recovery from idle-watcher suspensions so activation fails fast instead of hanging (#2405).
const MAX_REWAKES: u32 = 2;

/// Tracks fresh activation state while waiting for sandbox readiness.
pub(super) struct ActivationWait {
    record: SandboxRecord,
    /// The record passed in predates the wake, so its hibernated status isn't a re-hibernation.
    woke: bool,
    rewakes: u32,
    last_bump: Instant,
}

impl ActivationWait {
    pub(super) fn new(record: SandboxRecord, woke: bool) -> Self {
        Self {
            record,
            woke,
            rewakes: 0,
            last_bump: Instant::now(),
        }
    }

    fn needs_rewake(&self) -> bool {
        // `assess` reports replicas == Some(0) as "hibernated"; check only fresh records.
        if self.woke {
            return false;
        }
        if self.record.sandbox_name.is_none() {
            return false;
        }
        self.record.replicas == Some(0)
    }
}

impl SandboxManager {
    pub(super) async fn wait_until_ready(
        &self,
        id: &str,
        mut state: ActivationWait,
        context: &WaitContext<'_>,
    ) -> Result<SandboxRecord> {
        let ready = context.with_operation("wait_until_ready");
        loop {
            let status = assess(&state.record);
            if status.ready {
                return Ok(state.record);
            }
            self.recover_if_suspended(id, &mut state, context).await?;
            ensure_waitable(id, status)?;
            self.keepalive_if_due(id, &mut state, context).await;
            // After any recovery wake, wait and fetch rather than rechecking the
            // same suspended record and consuming another recovery attempt.
            self.wait(&ready, self.config.poll_interval).await?;
            state.record = self
                .retry_api(&ready, || self.api.get(id))
                .await?
                .with_context(|| format!("sandbox not found: {id}"))?;
            state.woke = false;
        }
    }

    async fn recover_if_suspended(
        &self,
        id: &str,
        state: &mut ActivationWait,
        context: &WaitContext<'_>,
    ) -> Result<()> {
        if !state.needs_rewake() {
            return Ok(());
        }
        let rewakes = state.rewakes;
        if rewakes >= MAX_REWAKES {
            anyhow::bail!(
                "sandbox {id} was suspended again while waking after {rewakes} recovery wake attempts"
            );
        }
        let activity = context.with_operation("bump_activity");
        self.retry_api(&activity, || self.api.bump_activity(id, Utc::now()))
            .await?;
        state.last_bump = Instant::now();
        self.wake_if_hibernated(id, &state.record, context).await?;
        state.rewakes += 1;
        log::warn!(
            "sandbox {id} was suspended again while waking; re-woke it ({}/{MAX_REWAKES} recovery wakes)",
            state.rewakes
        );
        Ok(())
    }

    async fn keepalive_if_due(
        &self,
        id: &str,
        state: &mut ActivationWait,
        context: &WaitContext<'_>,
    ) {
        if state.last_bump.elapsed() < KEEPALIVE_INTERVAL {
            return;
        }
        let activity = context.with_operation("bump_activity");
        if let Err(error) = self
            .retry_api(&activity, || self.api.bump_activity(id, Utc::now()))
            .await
        {
            log::warn!("failed to keep waking sandbox '{id}' active: {error:#}");
        }
        state.last_bump = Instant::now();
    }
}
