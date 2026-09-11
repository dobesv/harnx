use super::{SandboxManager, SandboxRecord};
use crate::lifecycle::wait::WaitContext;
use chrono::{DateTime, Utc};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const CREATION_GRACE_PERIOD: Duration = Duration::from_secs(2 * 60);

enum Recheck {
    Current(SandboxRecord),
    Skip,
    Cancelled,
}

impl SandboxManager {
    pub async fn run_idle_watcher(
        &self,
        cancel: CancellationToken,
        hibernated: tokio::sync::mpsc::UnboundedSender<String>,
    ) {
        let mut ticker = tokio::time::interval(self.config.scan_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = ticker.tick() => self.report_idle_scan(&cancel, &hibernated).await,
            }
        }
    }

    async fn report_idle_scan(
        &self,
        cancel: &CancellationToken,
        hibernated: &tokio::sync::mpsc::UnboundedSender<String>,
    ) {
        for sandbox_id in self.scan_idle_with_cancel(cancel).await {
            let _ = hibernated.send(sandbox_id);
        }
    }

    #[cfg(test)]
    pub(super) async fn scan_idle(&self) -> Vec<String> {
        self.scan_idle_with_cancel(&CancellationToken::new()).await
    }

    async fn scan_idle_with_cancel(&self, cancel: &CancellationToken) -> Vec<String> {
        let mut hibernated = Vec::new();
        let Some(records) = self.idle_records(cancel).await else {
            return hibernated;
        };
        let now = Utc::now();
        for record in records {
            if !should_hibernate(&record, now, self.config.idle_timeout) {
                continue;
            }
            let fresh = match self.recheck_idle(record, cancel).await {
                Recheck::Current(record) => record,
                Recheck::Skip => continue,
                Recheck::Cancelled => return hibernated,
            };
            if should_hibernate(&fresh, Utc::now(), self.config.idle_timeout)
                && self.hibernate_idle(&fresh, cancel).await
            {
                hibernated.push(fresh.id);
            }
        }
        hibernated
    }

    async fn idle_records(&self, cancel: &CancellationToken) -> Option<Vec<SandboxRecord>> {
        let context = WaitContext::unbounded("idle_list", cancel);
        match self.cancellable_api(&context, self.api.list()).await {
            Ok(records) => Some(records),
            Err(error) => {
                if !cancel.is_cancelled() {
                    log::warn!("sandbox watcher could not list claims: {error:#}");
                }
                None
            }
        }
    }

    async fn recheck_idle(&self, record: SandboxRecord, cancel: &CancellationToken) -> Recheck {
        let context = WaitContext::unbounded("idle_recheck", cancel);
        match self
            .cancellable_api(&context, self.api.get(&record.id))
            .await
        {
            Ok(Some(record)) => Recheck::Current(record),
            Ok(None) => Recheck::Skip,
            Err(_) if cancel.is_cancelled() => Recheck::Cancelled,
            Err(error) => {
                log::warn!(
                    "sandbox watcher could not recheck '{}': {error:#}",
                    record.id
                );
                Recheck::Skip
            }
        }
    }

    async fn hibernate_idle(&self, record: &SandboxRecord, cancel: &CancellationToken) -> bool {
        let context = WaitContext::unbounded("idle_hibernate", cancel);
        if let Err(error) = self
            .cancellable_api(&context, self.api.set_replicas(&record.id, 0))
            .await
        {
            log::warn!(
                "sandbox watcher could not hibernate '{}': {error:#}",
                record.id
            );
            return false;
        }
        metrics::counter!("harnx_sandbox_hibernations_total", "reason" => "idle").increment(1);
        true
    }
}

fn should_hibernate(record: &SandboxRecord, now: DateTime<Utc>, idle: Duration) -> bool {
    let Ok(grace) = chrono::Duration::from_std(CREATION_GRACE_PERIOD) else {
        return false;
    };
    let Ok(idle) = chrono::Duration::from_std(idle) else {
        return false;
    };
    let Some(created_at) = record.created_at else {
        return false;
    };
    let last_activity = record.last_activity.unwrap_or(created_at);
    now - created_at >= grace
        && now - last_activity > idle
        && record.replicas.is_some_and(|n| n > 0)
}
