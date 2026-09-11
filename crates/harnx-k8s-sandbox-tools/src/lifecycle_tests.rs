use super::*;
use parking_lot::Mutex;
use std::collections::{BTreeMap, VecDeque};

#[derive(Default)]
struct MockState {
    records: BTreeMap<String, SandboxRecord>,
    creates: Vec<String>,
    replica_updates: Vec<(String, i64)>,
    shutdown_updates: Vec<String>,
    activity_updates: Vec<String>,
    deletes: Vec<String>,
    get_errors_remaining: usize,
    get_calls: usize,
    get_error_calls: VecDeque<usize>,
    scripted_get_errors: VecDeque<anyhow::Error>,
    scripted_gets: VecDeque<Option<SandboxRecord>>,
    list_calls: usize,
    hold_list_call: Option<usize>,
}

#[derive(Default)]
struct MockApi {
    state: Mutex<MockState>,
    list_started: tokio::sync::Notify,
}

impl MockApi {
    fn insert(&self, record: SandboxRecord) {
        self.state.lock().records.insert(record.id.clone(), record);
    }
}

#[async_trait]
impl SandboxApi for MockApi {
    async fn create_claim(&self, request: CreateSandboxClaim) -> Result<String> {
        let mut state = self.state.lock();
        state.creates.push(request.name.clone());
        state
            .records
            .entry(request.name.clone())
            .or_insert_with(|| {
                record(
                    &request.name,
                    Some(1),
                    Some(request.shutdown_time),
                    Utc::now(),
                )
            });
        Ok(request.name)
    }

    async fn get(&self, id: &str) -> Result<Option<SandboxRecord>> {
        let mut state = self.state.lock();
        state.get_calls += 1;
        let get_call = state.get_calls;
        if let Some(error) = state.scripted_get_errors.pop_front() {
            return Err(error);
        }
        if state.get_errors_remaining > 0 {
            state.get_errors_remaining -= 1;
            anyhow::bail!("transient Kubernetes error");
        }
        if state.get_error_calls.front() == Some(&get_call) {
            state.get_error_calls.pop_front();
            anyhow::bail!("transient Kubernetes error");
        }
        if let Some(record) = state.scripted_gets.pop_front() {
            return Ok(record);
        }
        Ok(state.records.get(id).cloned())
    }

    async fn list(&self) -> Result<Vec<SandboxRecord>> {
        let (records, hold) = {
            let mut state = self.state.lock();
            state.list_calls += 1;
            (
                state.records.values().cloned().collect(),
                state.hold_list_call == Some(state.list_calls),
            )
        };
        self.list_started.notify_one();
        if hold {
            std::future::pending().await
        } else {
            Ok(records)
        }
    }

    async fn set_replicas(&self, id: &str, replicas: i64) -> Result<()> {
        let mut state = self.state.lock();
        state.replica_updates.push((id.to_string(), replicas));
        if let Some(record) = state.records.get_mut(id) {
            record.replicas = Some(replicas);
        }
        Ok(())
    }

    async fn update_shutdown_time(&self, id: &str, shutdown_time: DateTime<Utc>) -> Result<()> {
        let mut state = self.state.lock();
        state.shutdown_updates.push(id.to_string());
        if let Some(record) = state.records.get_mut(id) {
            record.shutdown_time = Some(shutdown_time);
        }
        Ok(())
    }

    async fn bump_activity(&self, id: &str, now: DateTime<Utc>) -> Result<()> {
        let mut state = self.state.lock();
        state.activity_updates.push(id.to_string());
        if let Some(record) = state.records.get_mut(id) {
            record.last_activity = Some(now);
        }
        Ok(())
    }

    async fn delete(&self, id: &str) -> Result<()> {
        let mut state = self.state.lock();
        state.deletes.push(id.to_string());
        state.records.remove(id);
        Ok(())
    }
}

fn record(
    id: &str,
    replicas: Option<i64>,
    shutdown_time: Option<DateTime<Utc>>,
    last_activity: DateTime<Utc>,
) -> SandboxRecord {
    SandboxRecord {
        id: id.to_string(),
        sandbox_name: Some(format!("sandbox-{id}")),
        pod_ips: vec!["10.0.0.8".to_string()],
        replicas,
        conditions: vec![SandboxCondition {
            kind: "Ready".to_string(),
            status: "True".to_string(),
            reason: String::new(),
            message: String::new(),
        }],
        shutdown_time,
        created_at: Some(last_activity - chrono::Duration::hours(1)),
        last_activity: Some(last_activity),
    }
}

fn test_manager(api: Arc<MockApi>) -> SandboxManager {
    SandboxManager::new(
        api,
        SandboxManagerConfig {
            poll_interval: Duration::from_millis(1),
            activation_timeout: Duration::from_secs(1),
            pod_ip_timeout: Duration::from_secs(1),
            idle_timeout: Duration::from_secs(60),
            ..SandboxManagerConfig::default()
        },
    )
}

fn pending_record(id: &str) -> SandboxRecord {
    let mut record = record(id, Some(1), None, Utc::now());
    record.conditions = Vec::new();
    record
}

fn assert_status(status: &SandboxStatus, expected: (&str, bool, bool, bool)) {
    assert_eq!(
        (
            status.state.as_str(),
            status.ready,
            status.terminal,
            status.error_message.is_some(),
        ),
        expected
    );
}

struct ZeroJitter;

fn typed_api_error(code: u16, retry_after_seconds: u32) -> anyhow::Error {
    let reason = if code == 429 {
        "TooManyRequests"
    } else {
        "Forbidden"
    };
    let mut status = kube::error::Status::failure("scripted Kubernetes error", reason)
        .with_code(code)
        .boxed();
    status.details = Some(kube::core::response::StatusDetails {
        name: String::new(),
        group: String::new(),
        kind: String::new(),
        uid: String::new(),
        causes: Vec::new(),
        retry_after_seconds,
    });
    crate::kubernetes::scripted_kubernetes_error(kube::Error::Api(status))
}

impl crate::policy::JitterSampler for ZeroJitter {
    fn sample(&self, _upper: Duration) -> Duration {
        Duration::ZERO
    }
}
#[path = "lifecycle_tests/activation.rs"]
mod activation;
#[path = "lifecycle_tests/idle.rs"]
mod idle;
#[path = "lifecycle_tests/mutations.rs"]
mod mutations;
#[path = "lifecycle_tests/retry.rs"]
mod retry;
#[path = "lifecycle_tests/status.rs"]
mod status;
