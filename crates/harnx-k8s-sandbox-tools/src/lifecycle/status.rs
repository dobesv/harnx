use super::{SandboxCondition, SandboxRecord, SandboxStatus};
use anyhow::Result;
use chrono::{DateTime, Utc};

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

pub(super) fn assess(record: &SandboxRecord) -> SandboxStatus {
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

pub(super) fn ensure_waitable(id: &str, status: SandboxStatus) -> Result<()> {
    if status.state != "error" && status.state != "deleted" {
        return Ok(());
    }
    anyhow::bail!(
        "sandbox {id} reached terminal state '{}': {}",
        status.state,
        status.error_message.unwrap_or_default()
    )
}

pub(super) fn deleted_status(id: &str) -> SandboxStatus {
    basic_status(id, "deleted", true)
}

pub(super) fn pending_status(id: &str) -> SandboxStatus {
    basic_status(id, "pending", false)
}

fn basic_status(id: &str, state: &str, terminal: bool) -> SandboxStatus {
    SandboxStatus {
        sandbox_id: id.to_string(),
        ready: false,
        state: state.to_string(),
        terminal,
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

pub(super) fn is_false(value: &bool) -> bool {
    !*value
}
