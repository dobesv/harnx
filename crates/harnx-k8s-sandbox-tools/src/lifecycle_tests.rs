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
    scripted_gets: VecDeque<Option<SandboxRecord>>,
}

#[derive(Default)]
struct MockApi {
    state: Mutex<MockState>,
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
        Ok(self.state.lock().records.values().cloned().collect())
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
    let (state, ready, terminal, has_error) = expected;
    assert_eq!(status.state, state);
    assert_eq!(status.ready, ready);
    assert_eq!(status.terminal, terminal);
    assert_eq!(status.error_message.is_some(), has_error);
}

#[test]
fn lifecycle_state_assessment_matches_tartarus_and_represents_hibernation() {
    let pending = pending_record("pending");
    let pending_status = assess(&pending);
    assert_status(&pending_status, ("pending", false, false, false));

    let ready_status = assess(&record("ready", Some(1), None, Utc::now()));
    assert_status(&ready_status, ("ready", true, true, false));

    for (reason, message) in [
        ("CreateError", ""),
        ("Pending", "failed to provision"),
        ("Pending", "access denied"),
    ] {
        let mut failed = pending_record("failed");
        failed.conditions = vec![SandboxCondition {
            kind: "Ready".to_string(),
            status: "False".to_string(),
            reason: reason.to_string(),
            message: message.to_string(),
        }];
        let status = assess(&failed);
        assert_status(&status, ("error", false, true, true));
    }

    let hibernated = assess(&record("sleeping", Some(0), None, Utc::now()));
    assert_status(&hibernated, ("hibernated", false, true, false));

    assert_status(&deleted_status("gone"), ("deleted", false, true, false));
}

#[tokio::test]
async fn status_observes_hibernation_without_waking_or_touching_activity() {
    let api = Arc::new(MockApi::default());
    api.insert(record("claim-1", Some(0), None, Utc::now()));
    let status = test_manager(api.clone())
        .status("claim-1", None)
        .await
        .unwrap();

    assert_eq!(status.state, "hibernated");
    assert!(status.terminal);
    let state = api.state.lock();
    assert!(state.replica_updates.is_empty());
    assert!(state.activity_updates.is_empty());
    assert!(state.shutdown_updates.is_empty());
}

#[tokio::test]
async fn ensure_active_wakes_extends_and_records_activity() {
    let api = Arc::new(MockApi::default());
    api.insert(record(
        "claim-2",
        Some(0),
        Some(Utc::now() + chrono::Duration::minutes(10)),
        Utc::now() - chrono::Duration::hours(1),
    ));

    let ip = test_manager(api.clone())
        .ensure_active("claim-2")
        .await
        .unwrap();

    assert_eq!(ip, "10.0.0.8");
    let state = api.state.lock();
    assert_eq!(state.replica_updates, [("claim-2".to_string(), 1)]);
    assert_eq!(state.shutdown_updates, ["claim-2"]);
    assert_eq!(state.activity_updates, ["claim-2"]);
}

#[tokio::test]
async fn status_wait_retries_transient_api_errors() {
    let api = Arc::new(MockApi::default());
    api.insert(record("claim-ready", Some(1), None, Utc::now()));
    api.state.lock().get_errors_remaining = 2;

    let status = test_manager(api)
        .status("claim-ready", Some(Duration::from_secs(1)))
        .await
        .unwrap();

    assert_eq!(status.state, "ready");
    assert!(!status.timed_out);
}

#[tokio::test]
async fn status_wait_times_out_with_the_last_observed_state() {
    let api = Arc::new(MockApi::default());
    api.insert(pending_record("claim-pending"));
    let status = test_manager(api)
        .status("claim-pending", Some(Duration::from_millis(3)))
        .await
        .unwrap();

    assert_eq!(status.state, "pending");
    assert!(status.timed_out);
    assert!(!status.terminal);
}

#[tokio::test]
async fn status_wait_times_out_after_only_transient_api_errors() {
    let api = Arc::new(MockApi::default());
    api.state.lock().get_errors_remaining = usize::MAX;
    let status = test_manager(api)
        .status("claim-unknown", Some(Duration::from_millis(3)))
        .await
        .unwrap();

    assert_eq!(status.sandbox_id, "claim-unknown");
    assert_eq!(status.state, "pending");
    assert!(status.timed_out);
}

#[tokio::test]
async fn ensure_active_waits_for_a_pod_ip_after_readiness() {
    let api = Arc::new(MockApi::default());
    let mut without_ip = record("claim-ip", Some(1), None, Utc::now());
    without_ip.pod_ips.clear();
    let with_ip = record("claim-ip", Some(1), None, Utc::now());
    api.state
        .lock()
        .scripted_gets
        .extend([Some(without_ip), Some(with_ip)]);

    let ip = test_manager(api).ensure_active("claim-ip").await.unwrap();
    assert_eq!(ip, "10.0.0.8");
}

#[tokio::test]
async fn ensure_active_retries_transient_errors_during_each_wait_phase() {
    let api = Arc::new(MockApi::default());
    let pending = pending_record("claim-retry");
    let mut ready_without_ip = record("claim-retry", Some(1), None, Utc::now());
    ready_without_ip.pod_ips.clear();
    let ready_with_ip = record("claim-retry", Some(1), None, Utc::now());
    {
        let mut state = api.state.lock();
        state.get_error_calls.extend([1, 3, 5]);
        state
            .scripted_gets
            .extend([Some(pending), Some(ready_without_ip), Some(ready_with_ip)]);
    }

    let ip = test_manager(api.clone())
        .ensure_active("claim-retry")
        .await
        .unwrap();

    assert_eq!(ip, "10.0.0.8");
    assert_eq!(api.state.lock().get_calls, 6);
}

#[tokio::test]
async fn ensure_active_reports_a_missing_pod_ip_after_its_budget() {
    let api = Arc::new(MockApi::default());
    let mut without_ip = record("claim-no-ip", Some(1), None, Utc::now());
    without_ip.pod_ips.clear();
    api.insert(without_ip);
    let manager = SandboxManager::new(
        api,
        SandboxManagerConfig {
            poll_interval: Duration::from_millis(1),
            activation_timeout: Duration::from_secs(1),
            pod_ip_timeout: Duration::from_millis(3),
            ..SandboxManagerConfig::default()
        },
    );

    let error = manager.ensure_active("claim-no-ip").await.unwrap_err();
    assert!(error.to_string().contains("has no pod IP"));
}

#[tokio::test]
async fn release_hibernates_or_destroys_the_claim() {
    let api = Arc::new(MockApi::default());
    api.insert(record("claim-release", Some(1), None, Utc::now()));
    let manager = test_manager(api.clone());

    assert_eq!(
        manager.release("claim-release", false).await.unwrap(),
        "hibernated"
    );
    assert_eq!(
        api.state.lock().replica_updates,
        [("claim-release".to_string(), 0)]
    );

    assert_eq!(
        manager.release("claim-release", true).await.unwrap(),
        "destroyed"
    );
    assert_eq!(api.state.lock().deletes, ["claim-release"]);
}

#[tokio::test]
async fn idle_scan_hibernates_only_running_inactive_sandboxes() {
    let api = Arc::new(MockApi::default());
    api.insert(record(
        "idle",
        Some(1),
        None,
        Utc::now() - chrono::Duration::hours(1),
    ));
    api.insert(record("active", Some(1), None, Utc::now()));
    api.insert(record(
        "already-asleep",
        Some(0),
        None,
        Utc::now() - chrono::Duration::hours(1),
    ));

    let hibernated = test_manager(api.clone()).scan_idle().await;

    assert_eq!(api.state.lock().replica_updates, [("idle".to_string(), 0)]);
    assert_eq!(hibernated, ["idle"]);
}

#[tokio::test]
async fn idle_scan_rechecks_activity_before_hibernating() {
    let api = Arc::new(MockApi::default());
    api.insert(record(
        "became-active",
        Some(1),
        None,
        Utc::now() - chrono::Duration::hours(1),
    ));
    api.state.lock().scripted_gets.push_back(Some(record(
        "became-active",
        Some(1),
        None,
        Utc::now(),
    )));

    let hibernated = test_manager(api.clone()).scan_idle().await;

    assert!(hibernated.is_empty());
    assert!(api.state.lock().replica_updates.is_empty());
}

#[tokio::test]
async fn create_uses_a_stable_dns_safe_name_for_retries() {
    let api = Arc::new(MockApi::default());
    let manager = test_manager(api.clone());

    let first = manager.create("CALL_123/a", None).await.unwrap();
    let second = manager.create("CALL_123/a", None).await.unwrap();

    assert!(first.starts_with("sandbox-"));
    assert_eq!(first.len(), 40);
    assert_eq!(second, first);
    assert_eq!(api.state.lock().creates, [first.clone(), first]);
}
