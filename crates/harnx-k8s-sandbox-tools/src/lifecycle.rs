use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

// Retain Tartarus's annotation during migration so either watcher observes
// activity written by the other implementation.
pub const LAST_ACTIVITY_ANNOTATION: &str = "kagent/last-activity";
const CREATION_GRACE_PERIOD: Duration = Duration::from_secs(2 * 60);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxCondition {
    pub kind: String,
    pub status: String,
    pub reason: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SandboxRecord {
    pub id: String,
    pub sandbox_name: Option<String>,
    pub pod_ips: Vec<String>,
    pub replicas: Option<i64>,
    pub conditions: Vec<SandboxCondition>,
    pub shutdown_time: Option<DateTime<Utc>>,
    pub created_at: Option<DateTime<Utc>>,
    pub last_activity: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SandboxStatus {
    pub sandbox_id: String,
    pub ready: bool,
    pub state: String,
    pub terminal: bool,
    pub conditions: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shutdown_time: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seconds_remaining: Option<i64>,
    #[serde(skip_serializing_if = "is_false")]
    pub timed_out: bool,
}

pub struct CreateSandboxClaim {
    pub name: String,
    pub template: String,
    pub shutdown_time: DateTime<Utc>,
    pub description: Option<String>,
}

#[async_trait]
pub trait SandboxApi: Send + Sync {
    async fn create_claim(&self, claim: CreateSandboxClaim) -> Result<String>;
    async fn get(&self, id: &str) -> Result<Option<SandboxRecord>>;
    async fn list(&self) -> Result<Vec<SandboxRecord>>;
    async fn set_replicas(&self, id: &str, replicas: i64) -> Result<()>;
    async fn update_shutdown_time(&self, id: &str, shutdown_time: DateTime<Utc>) -> Result<()>;
    async fn bump_activity(&self, id: &str, now: DateTime<Utc>) -> Result<()>;
    async fn delete(&self, id: &str) -> Result<()>;
}

#[derive(Clone, Debug)]
pub struct SandboxManagerConfig {
    pub template: String,
    pub default_ttl: Duration,
    pub activation_timeout: Duration,
    pub pod_ip_timeout: Duration,
    pub poll_interval: Duration,
    pub auto_extend_threshold: Duration,
    pub auto_extend_ttl: Duration,
    pub scan_interval: Duration,
    pub idle_timeout: Duration,
}

impl Default for SandboxManagerConfig {
    fn default() -> Self {
        Self {
            template: "formative-buildbox".to_string(),
            default_ttl: Duration::from_secs(72 * 60 * 60),
            activation_timeout: Duration::from_secs(30 * 60),
            pod_ip_timeout: Duration::from_secs(15),
            poll_interval: Duration::from_secs(5),
            auto_extend_threshold: Duration::from_secs(48 * 60 * 60),
            auto_extend_ttl: Duration::from_secs(72 * 60 * 60),
            scan_interval: Duration::from_secs(15 * 60),
            idle_timeout: Duration::from_secs(15 * 60),
        }
    }
}

#[derive(Clone)]
pub struct SandboxManager {
    api: Arc<dyn SandboxApi>,
    config: SandboxManagerConfig,
}

impl SandboxManager {
    pub fn new(api: Arc<dyn SandboxApi>, config: SandboxManagerConfig) -> Self {
        Self { api, config }
    }

    pub async fn create(&self, call_id: &str, description: Option<&str>) -> Result<String> {
        let shutdown_time = Utc::now()
            + chrono::Duration::from_std(self.config.default_ttl)
                .context("default sandbox TTL is out of range")?;
        self.api
            .create_claim(CreateSandboxClaim {
                name: claim_name(call_id),
                template: self.config.template.clone(),
                shutdown_time,
                description: description.map(str::to_string),
            })
            .await
    }

    pub async fn ensure_active(&self, id: &str) -> Result<String> {
        let deadline = tokio::time::Instant::now() + self.config.activation_timeout;
        let record = self
            .get_during_wait(id, deadline)
            .await?
            .with_context(|| format!("sandbox not found: {id}"))?;
        self.maybe_extend(&record).await;
        self.wake_if_hibernated(id, &record).await?;
        let record = self.wait_until_ready(id, record, deadline).await?;
        self.api.bump_activity(id, Utc::now()).await?;
        self.wait_for_pod_ip(id, record).await
    }

    async fn wake_if_hibernated(&self, id: &str, record: &SandboxRecord) -> Result<()> {
        if record.sandbox_name.is_none() || record.replicas != Some(0) {
            return Ok(());
        }
        self.api.set_replicas(id, 1).await?;
        metrics::counter!("harnx_sandbox_wakes_total").increment(1);
        Ok(())
    }

    async fn wait_until_ready(
        &self,
        id: &str,
        mut record: SandboxRecord,
        deadline: tokio::time::Instant,
    ) -> Result<SandboxRecord> {
        loop {
            let status = assess(&record);
            if status.ready {
                return Ok(record);
            }
            ensure_waitable(id, status)?;
            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!(
                    "sandbox {id} not ready after {:?}",
                    self.config.activation_timeout
                );
            }
            tokio::time::sleep(self.config.poll_interval).await;
            record = self
                .get_during_wait(id, deadline)
                .await?
                .with_context(|| format!("sandbox not found: {id}"))?;
        }
    }

    async fn wait_for_pod_ip(&self, id: &str, mut record: SandboxRecord) -> Result<String> {
        let deadline = tokio::time::Instant::now() + self.config.pod_ip_timeout;
        loop {
            if let Some(ip) = record.pod_ips.first() {
                return Ok(ip.clone());
            }
            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!("sandbox {id} has no pod IP yet — it may still be starting");
            }
            tokio::time::sleep(self.config.poll_interval).await;
            record = self
                .get_during_wait(id, deadline)
                .await?
                .with_context(|| format!("sandbox not found: {id}"))?;
        }
    }

    async fn get_during_wait(
        &self,
        id: &str,
        deadline: tokio::time::Instant,
    ) -> Result<Option<SandboxRecord>> {
        loop {
            match self.api.get(id).await {
                Ok(record) => return Ok(record),
                Err(error) if tokio::time::Instant::now() >= deadline => {
                    return Err(error)
                        .with_context(|| format!("read sandbox '{id}' before wait deadline"));
                }
                Err(error) => {
                    log::debug!(
                        "retry sandbox '{id}' read after transient Kubernetes error: {error:#}"
                    );
                    tokio::time::sleep(self.config.poll_interval).await;
                }
            }
        }
    }

    async fn maybe_extend(&self, record: &SandboxRecord) {
        let Some(shutdown_time) = record.shutdown_time else {
            return;
        };
        let Ok(threshold) = chrono::Duration::from_std(self.config.auto_extend_threshold) else {
            return;
        };
        if shutdown_time - Utc::now() >= threshold {
            return;
        }
        let Ok(ttl) = chrono::Duration::from_std(self.config.auto_extend_ttl) else {
            return;
        };
        if let Err(error) = self
            .api
            .update_shutdown_time(&record.id, Utc::now() + ttl)
            .await
        {
            log::warn!("failed to auto-extend sandbox '{}': {error:#}", record.id);
        }
    }

    pub async fn status(&self, id: &str, timeout: Option<Duration>) -> Result<SandboxStatus> {
        let deadline = timeout.map(|timeout| tokio::time::Instant::now() + timeout);
        let mut last_status = pending_status(id);
        loop {
            match self.api.get(id).await {
                Ok(record) => {
                    let status = record.as_ref().map_or_else(|| deleted_status(id), assess);
                    if status.terminal || deadline.is_none() {
                        return Ok(status);
                    }
                    last_status = status;
                }
                Err(error) if deadline.is_none() => return Err(error),
                Err(error) => {
                    // Match Tartarus's bounded-wait behavior: transient API
                    // failures are retried until the caller's deadline.
                    log::debug!("retry sandbox '{id}' status after Kubernetes error: {error:#}");
                }
            }
            if deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
                last_status.timed_out = true;
                return Ok(last_status);
            }
            tokio::time::sleep(self.config.poll_interval).await;
        }
    }

    pub async fn release(&self, id: &str, destroy: bool) -> Result<&'static str> {
        if destroy {
            self.api.delete(id).await?;
            Ok("destroyed")
        } else {
            self.api.set_replicas(id, 0).await?;
            metrics::counter!("harnx_sandbox_hibernations_total", "reason" => "release")
                .increment(1);
            Ok("hibernated")
        }
    }

    pub async fn record_activity(&self, id: &str) -> Result<()> {
        self.api.bump_activity(id, Utc::now()).await
    }

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
                _ = ticker.tick() => {
                    for sandbox_id in self.scan_idle().await {
                        let _ = hibernated.send(sandbox_id);
                    }
                },
            }
        }
    }

    async fn scan_idle(&self) -> Vec<String> {
        let mut hibernated = Vec::new();
        let records = match self.api.list().await {
            Ok(records) => records,
            Err(error) => {
                log::warn!("sandbox watcher could not list claims: {error:#}");
                return hibernated;
            }
        };
        let now = Utc::now();
        for record in records {
            if !should_hibernate(&record, now, self.config.idle_timeout) {
                continue;
            }
            let fresh = match self.api.get(&record.id).await {
                Ok(Some(record)) => record,
                Ok(None) => continue,
                Err(error) => {
                    log::warn!(
                        "sandbox watcher could not recheck '{}': {error:#}",
                        record.id
                    );
                    continue;
                }
            };
            if !should_hibernate(&fresh, Utc::now(), self.config.idle_timeout) {
                continue;
            }
            if let Err(error) = self.api.set_replicas(&fresh.id, 0).await {
                log::warn!(
                    "sandbox watcher could not hibernate '{}': {error:#}",
                    fresh.id
                );
            } else {
                metrics::counter!("harnx_sandbox_hibernations_total", "reason" => "idle")
                    .increment(1);
                hibernated.push(fresh.id);
            }
        }
        hibernated
    }
}

fn claim_name(call_id: &str) -> String {
    let mut name = "sandbox-".to_string();
    for byte in Sha256::digest(call_id.as_bytes()).iter().take(16) {
        write!(&mut name, "{byte:02x}").expect("writing to a string cannot fail");
    }
    name
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

#[derive(Default)]
struct Readiness {
    ready: bool,
    error_message: Option<String>,
}

impl Readiness {
    fn observe(&mut self, condition: &SandboxCondition) {
        match condition.status.as_str() {
            "True" => {
                self.ready = true;
                self.error_message = None;
            }
            "False" if terminal_failure(&condition.reason, &condition.message) => {
                self.error_message = Some(condition_error(condition));
            }
            _ => {}
        }
    }

    fn state(&self) -> &'static str {
        if self.ready {
            "ready"
        } else if self.error_message.is_some() {
            "error"
        } else {
            "pending"
        }
    }
}

fn readiness(conditions: &[SandboxCondition]) -> Readiness {
    let mut readiness = Readiness::default();
    for condition in conditions.iter().filter(|value| value.kind == "Ready") {
        readiness.observe(condition);
    }
    readiness
}

fn condition_error(condition: &SandboxCondition) -> String {
    if condition.message.is_empty() {
        condition.reason.clone()
    } else {
        condition.message.clone()
    }
}

fn assess(record: &SandboxRecord) -> SandboxStatus {
    if record.replicas == Some(0) {
        return SandboxStatus {
            sandbox_id: record.id.clone(),
            ready: false,
            state: "hibernated".to_string(),
            terminal: true,
            conditions: condition_summary(&record.conditions),
            error_message: None,
            shutdown_time: record.shutdown_time,
            seconds_remaining: remaining(record.shutdown_time),
            timed_out: false,
        };
    }

    let readiness = readiness(&record.conditions);
    SandboxStatus {
        sandbox_id: record.id.clone(),
        ready: readiness.ready,
        state: readiness.state().to_string(),
        terminal: readiness.ready || readiness.error_message.is_some(),
        conditions: condition_summary(&record.conditions),
        error_message: readiness.error_message,
        shutdown_time: record.shutdown_time,
        seconds_remaining: remaining(record.shutdown_time),
        timed_out: false,
    }
}

fn ensure_waitable(id: &str, status: SandboxStatus) -> Result<()> {
    if status.state != "error" && status.state != "deleted" {
        return Ok(());
    }
    anyhow::bail!(
        "sandbox {id} reached terminal state '{}': {}",
        status.state,
        status.error_message.unwrap_or_default()
    )
}

fn deleted_status(id: &str) -> SandboxStatus {
    SandboxStatus {
        sandbox_id: id.to_string(),
        ready: false,
        state: "deleted".to_string(),
        terminal: true,
        conditions: String::new(),
        error_message: None,
        shutdown_time: None,
        seconds_remaining: None,
        timed_out: false,
    }
}

fn pending_status(id: &str) -> SandboxStatus {
    SandboxStatus {
        sandbox_id: id.to_string(),
        ready: false,
        state: "pending".to_string(),
        terminal: false,
        conditions: String::new(),
        error_message: None,
        shutdown_time: None,
        seconds_remaining: None,
        timed_out: false,
    }
}

fn condition_summary(conditions: &[SandboxCondition]) -> String {
    conditions
        .iter()
        .filter(|condition| !condition.kind.is_empty())
        .map(|condition| format!("{}={}", condition.kind, condition.status))
        .collect::<Vec<_>>()
        .join(", ")
}

fn remaining(shutdown_time: Option<DateTime<Utc>>) -> Option<i64> {
    shutdown_time.map(|time| (time - Utc::now()).num_seconds().max(0))
}

fn terminal_failure(reason: &str, message: &str) -> bool {
    const KEYWORDS: &[&str] = &[
        "error",
        "failed",
        "failure",
        "reconcileerror",
        "reconcilefailed",
        "podfailed",
        "poderror",
        "backoff",
        "crash",
        "denied",
    ];
    let text = format!("{reason} {message}").to_lowercase();
    KEYWORDS.iter().any(|keyword| text.contains(keyword))
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod tests;
