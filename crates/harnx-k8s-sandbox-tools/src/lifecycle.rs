use crate::policy::BackoffConfig;
#[cfg(test)]
use crate::policy::{EndReason, FailureKind, TerminalError};
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

mod idle;
mod status;
mod wait;

use status::{assess, deleted_status, ensure_waitable, pending_status};
use wait::{is_deadline, WaitContext};

// Retain Tartarus's annotation during migration so either watcher observes
// activity written by the other implementation.
pub const LAST_ACTIVITY_ANNOTATION: &str = "kagent/last-activity";

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
    #[serde(skip_serializing_if = "status::is_false")]
    pub timed_out: bool,
}

#[derive(Clone)]
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
    pub backoff_base: Duration,
    pub backoff_cap: Duration,
    pub max_attempts: usize,
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
            backoff_base: Duration::from_millis(250),
            backoff_cap: Duration::from_secs(10),
            max_attempts: 5,
        }
    }
}

#[derive(Clone)]
pub struct SandboxManager {
    api: Arc<dyn SandboxApi>,
    config: SandboxManagerConfig,
    backoff: BackoffConfig,
}

impl SandboxManager {
    pub fn new(api: Arc<dyn SandboxApi>, config: SandboxManagerConfig) -> Self {
        let backoff =
            BackoffConfig::new(config.backoff_base, config.backoff_cap, config.max_attempts);
        Self {
            api,
            config,
            backoff,
        }
    }

    #[cfg(test)]
    fn with_backoff(mut self, backoff: BackoffConfig) -> Self {
        self.backoff = backoff;
        self
    }

    pub async fn create(
        &self,
        call_id: &str,
        description: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<String> {
        let shutdown_time = Utc::now()
            + chrono::Duration::from_std(self.config.default_ttl)
                .context("default sandbox TTL is out of range")?;
        let request = CreateSandboxClaim {
            name: claim_name(call_id),
            template: self.config.template.clone(),
            shutdown_time,
            description: description.map(str::to_string),
        };
        let context = WaitContext::unbounded("create", cancel);
        self.retry_api(&context, || self.api.create_claim(request.clone()))
            .await
    }

    pub async fn ensure_active(&self, id: &str, cancel: &CancellationToken) -> Result<String> {
        let deadline = tokio::time::Instant::now() + self.config.activation_timeout;
        let context = WaitContext::new("ensure_active", cancel, deadline);
        let record = self
            .retry_api(&context, || self.api.get(id))
            .await?
            .with_context(|| format!("sandbox not found: {id}"))?;
        self.maybe_extend(&record, &context).await;
        self.wake_if_hibernated(id, &record, &context).await?;
        let record = self.wait_until_ready(id, record, &context).await?;
        let activity_time = Utc::now();
        let activity = context.with_operation("bump_activity");
        self.retry_api(&activity, || self.api.bump_activity(id, activity_time))
            .await?;
        self.wait_for_pod_ip(id, record, cancel).await
    }

    async fn wake_if_hibernated(
        &self,
        id: &str,
        record: &SandboxRecord,
        context: &WaitContext<'_>,
    ) -> Result<()> {
        if record.sandbox_name.is_none() || record.replicas != Some(0) {
            return Ok(());
        }
        let wake = context.with_operation("wake");
        self.retry_api(&wake, || self.api.set_replicas(id, 1))
            .await?;
        metrics::counter!("harnx_sandbox_wakes_total").increment(1);
        Ok(())
    }

    async fn wait_until_ready(
        &self,
        id: &str,
        mut record: SandboxRecord,
        context: &WaitContext<'_>,
    ) -> Result<SandboxRecord> {
        let ready = context.with_operation("wait_until_ready");
        loop {
            let status = assess(&record);
            if status.ready {
                return Ok(record);
            }
            ensure_waitable(id, status)?;
            self.wait(&ready, self.config.poll_interval).await?;
            record = self
                .retry_api(&ready, || self.api.get(id))
                .await?
                .with_context(|| format!("sandbox not found: {id}"))?;
        }
    }

    async fn wait_for_pod_ip(
        &self,
        id: &str,
        mut record: SandboxRecord,
        cancel: &CancellationToken,
    ) -> Result<String> {
        let deadline = tokio::time::Instant::now() + self.config.pod_ip_timeout;
        let context = WaitContext::new("wait_for_pod_ip", cancel, deadline);
        loop {
            if let Some(ip) = record.pod_ips.first() {
                return Ok(ip.clone());
            }
            self.wait(&context, self.config.poll_interval)
                .await
                .with_context(|| {
                    format!("sandbox {id} has no pod IP yet; it may still be starting")
                })?;
            record = self
                .retry_api(&context, || self.api.get(id))
                .await?
                .with_context(|| format!("sandbox not found: {id}"))?;
        }
    }

    async fn maybe_extend(&self, record: &SandboxRecord, context: &WaitContext<'_>) {
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
        let extend = context.with_operation("auto_extend");
        if let Err(error) = self
            .cancellable_api(
                &extend,
                self.api.update_shutdown_time(&record.id, Utc::now() + ttl),
            )
            .await
        {
            log::warn!("failed to auto-extend sandbox '{}': {error:#}", record.id);
        }
    }

    pub async fn status(
        &self,
        id: &str,
        timeout: Option<Duration>,
        cancel: &CancellationToken,
    ) -> Result<SandboxStatus> {
        let Some(timeout) = timeout else {
            return self.status_once(id, cancel).await;
        };
        let context = WaitContext::new("status", cancel, tokio::time::Instant::now() + timeout);
        self.status_until_terminal(id, &context).await
    }

    async fn status_once(&self, id: &str, cancel: &CancellationToken) -> Result<SandboxStatus> {
        let context = WaitContext::unbounded("status", cancel);
        let record = self.cancellable_api(&context, self.api.get(id)).await?;
        Ok(record.as_ref().map_or_else(|| deleted_status(id), assess))
    }

    async fn status_until_terminal(
        &self,
        id: &str,
        context: &WaitContext<'_>,
    ) -> Result<SandboxStatus> {
        let mut last_status = pending_status(id);
        loop {
            let read = self.retry_api(context, || self.api.get(id)).await;
            match read {
                Ok(record) => {
                    let status = record.as_ref().map_or_else(|| deleted_status(id), assess);
                    if status.terminal {
                        return Ok(status);
                    }
                    last_status = status;
                }
                Err(error) => return deadline_status(last_status, error),
            }
            if let Err(error) = self.wait(context, self.config.poll_interval).await {
                return deadline_status(last_status, error);
            }
        }
    }

    pub async fn release(
        &self,
        id: &str,
        destroy: bool,
        cancel: &CancellationToken,
    ) -> Result<&'static str> {
        if destroy {
            let context = WaitContext::unbounded("delete", cancel);
            self.retry_api(&context, || self.api.delete(id)).await?;
            Ok("destroyed")
        } else {
            let context = WaitContext::unbounded("hibernate", cancel);
            self.retry_api(&context, || self.api.set_replicas(id, 0))
                .await?;
            metrics::counter!("harnx_sandbox_hibernations_total", "reason" => "release")
                .increment(1);
            Ok("hibernated")
        }
    }

    pub async fn record_activity(&self, id: &str, cancel: &CancellationToken) -> Result<()> {
        let activity_time = Utc::now();
        let context = WaitContext::unbounded("heartbeat", cancel);
        self.retry_api(&context, || self.api.bump_activity(id, activity_time))
            .await
    }
}

fn deadline_status(mut last_status: SandboxStatus, error: anyhow::Error) -> Result<SandboxStatus> {
    if !is_deadline(&error) {
        return Err(error);
    }
    last_status.timed_out = true;
    Ok(last_status)
}
fn claim_name(call_id: &str) -> String {
    let mut name = "sandbox-".to_string();
    for byte in Sha256::digest(call_id.as_bytes()).iter().take(16) {
        write!(&mut name, "{byte:02x}").expect("writing to a string cannot fail");
    }
    name
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod tests;
