use super::*;

#[derive(Default)]
pub(super) struct ReadyApi {
    pub(super) seen_ids: Mutex<Vec<String>>,
    pub(super) missing_ids: Mutex<Vec<String>>,
    pub(super) scripted_gets: Mutex<VecDeque<Option<SandboxRecord>>>,
    pub(super) get_errors: Mutex<VecDeque<anyhow::Error>>,
    pub(super) create_errors: Mutex<VecDeque<anyhow::Error>>,
    /// Async side effects run at claim-creation time, after the connect has
    /// observed the session binding but before it can bind its claim —
    /// how tests script a concurrent writer replacing the binding.
    pub(super) create_hooks: Mutex<VecDeque<Pin<Box<dyn Future<Output = ()> + Send>>>>,
    pub(super) creates: Mutex<Vec<String>>,
    pub(super) activity: Mutex<Vec<String>>,
    pub(super) replica_updates: Mutex<Vec<(String, i64)>>,
    pub(super) deletes: Mutex<Vec<String>>,
    pub(super) delete_errors: Mutex<VecDeque<anyhow::Error>>,
    pub(super) hold_activity: AtomicBool,
    pub(super) activity_started: Notify,
}

#[async_trait]
impl SandboxApi for ReadyApi {
    async fn create_claim(&self, request: CreateSandboxClaim) -> Result<String> {
        self.creates.lock().push(request.name.clone());
        let hook = self.create_hooks.lock().pop_front();
        if let Some(hook) = hook {
            hook.await;
        }
        if let Some(error) = self.create_errors.lock().pop_front() {
            return Err(error);
        }
        Ok(request.name)
    }

    async fn get(&self, id: &str) -> Result<Option<SandboxRecord>> {
        self.seen_ids.lock().push(id.to_string());
        if let Some(record) = self.scripted_gets.lock().pop_front() {
            return Ok(record);
        }
        if let Some(error) = self.get_errors.lock().pop_front() {
            return Err(error);
        }
        if self.missing_ids.lock().iter().any(|missing| missing == id) {
            return Ok(None);
        }
        Ok(Some(SandboxRecord {
            id: id.to_string(),
            sandbox_name: Some(format!("pod-{id}")),
            pod_ips: vec!["10.0.0.8".to_string()],
            replicas: Some(1),
            conditions: vec![SandboxCondition {
                kind: "Ready".to_string(),
                status: "True".to_string(),
                reason: String::new(),
                message: String::new(),
            }],
            shutdown_time: None,
            created_at: Some(Utc::now()),
            last_activity: None,
        }))
    }

    async fn list(&self) -> Result<Vec<SandboxRecord>> {
        Ok(Vec::new())
    }

    async fn set_replicas(&self, id: &str, replicas: i64) -> Result<()> {
        self.replica_updates.lock().push((id.to_string(), replicas));
        Ok(())
    }

    async fn update_shutdown_time(
        &self,
        _id: &str,
        _shutdown_time: chrono::DateTime<Utc>,
    ) -> Result<()> {
        Ok(())
    }

    async fn bump_activity(&self, id: &str, _now: chrono::DateTime<Utc>) -> Result<()> {
        self.activity.lock().push(id.to_string());
        self.activity_started.notify_one();
        if self.hold_activity.load(Ordering::SeqCst) {
            std::future::pending().await
        } else {
            Ok(())
        }
    }

    async fn delete(&self, id: &str) -> Result<()> {
        self.deletes.lock().push(id.to_string());
        if let Some(error) = self.delete_errors.lock().pop_front() {
            return Err(error);
        }
        Ok(())
    }
}
