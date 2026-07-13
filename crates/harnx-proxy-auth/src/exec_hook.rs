#[cfg(not(unix))]
use anyhow::{anyhow, Result};
use serde_json::{Map, Value};
#[cfg(not(unix))]
use std::path::PathBuf;

#[cfg(unix)]
mod imp {
    use anyhow::{anyhow, bail, Context, Result};
    use serde::{Deserialize, Serialize};
    use serde_json::{Map, Value};
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    };
    use std::time::Duration;
    use tempfile::{Builder, TempDir};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::process::{Child, ChildStdin, Command};
    use tokio::sync::{oneshot, Mutex};
    use tokio::task::JoinHandle;
    use tracing::{debug, warn};

    static EVENT_COUNTER: AtomicU64 = AtomicU64::new(1);
    const STARTUP_READY_TIMEOUT: Duration = Duration::from_secs(2);

    type PendingMap = Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>;

    #[derive(Serialize)]
    struct JsonlRequest {
        id: String,
        /// Safe resolved vars (sentinels + temp_file_root) so exec hooks have
        /// the same context jq hooks resolve. Omitted when empty.
        #[serde(skip_serializing_if = "Value::is_null")]
        vars: Value,
        #[serde(flatten)]
        req: Value,
    }

    #[derive(Serialize)]
    struct StartupRequest {
        id: String,
        event: &'static str,
        vars: Value,
    }

    #[derive(Deserialize)]
    struct JsonlResponse {
        id: String,
        #[serde(flatten)]
        req: Value,
    }

    /// Process one line of an exec hook's stdout. A standalone
    /// `{"notice": {...}}` line bubbles up to harnx (returns `false`); otherwise
    /// the line is a transform response and resolves its pending request
    /// (returns `true`).
    async fn resolve_exec_line(trimmed: &str, pending: &PendingMap) -> bool {
        if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
            if let Some(notice) = crate::notice::parse_notice_line(&value) {
                crate::notice::send(&notice.level, &notice.message);
                return false;
            }
        }
        match serde_json::from_str::<JsonlResponse>(trimmed) {
            Ok(response) => {
                let mut map = pending.lock().await;
                if let Some(sender) = map.remove(&response.id) {
                    let _ = sender.send(response.req);
                } else {
                    warn!(
                        "Exec hook returned response for unknown request id `{}`",
                        response.id
                    );
                }
            }
            Err(err) => {
                warn!("Failed to parse exec hook response: {err}");
            }
        }
        true
    }

    pub struct ExecHookProcess {
        source: ExecSource,
        timeout: Duration,
        request_vars: Arc<Value>,
        state: Mutex<Option<ProcessRuntime>>,
    }

    #[derive(Clone)]
    enum ExecSource {
        Inline(Arc<String>),
        Path(PathBuf),
    }

    struct ProcessRuntime {
        _child: Child,
        stdin: Arc<Mutex<ChildStdin>>,
        pending: PendingMap,
        _reader_task: JoinHandle<()>,
        _stderr_task: JoinHandle<()>,
        _script_dir: Option<TempDir>,
    }

    struct RuntimeHandle {
        stdin: Arc<Mutex<ChildStdin>>,
        pending: PendingMap,
    }

    impl ExecHookProcess {
        pub fn spawn_inline(script: &str, timeout_secs: u64) -> Result<Self> {
            Ok(Self {
                source: ExecSource::Inline(Arc::new(script.to_owned())),
                timeout: Duration::from_secs(timeout_secs),
                request_vars: Arc::new(Value::Null),
                state: Mutex::new(None),
            })
        }

        pub fn spawn_path(path: PathBuf, timeout_secs: u64) -> Result<Self> {
            validate_exec_path(&path)?;
            Ok(Self {
                source: ExecSource::Path(path),
                timeout: Duration::from_secs(timeout_secs),
                request_vars: Arc::new(Value::Null),
                state: Mutex::new(None),
            })
        }

        /// Attach the safe resolved vars (sentinels + `temp_file_root`) that get
        /// sent to the hook on every request. Defaults to none.
        pub fn with_request_vars(mut self, request_vars: Arc<Value>) -> Self {
            self.request_vars = request_vars;
            self
        }

        pub async fn transform(&self, req: Value) -> Value {
            let original = req.clone();
            let runtime = match self.ensure_runtime().await {
                Ok(runtime) => runtime,
                Err(err) => {
                    warn!("Exec hook spawn failed: {err}");
                    return original;
                }
            };

            let id = format!("evt-{}", EVENT_COUNTER.fetch_add(1, Ordering::Relaxed));
            let request = JsonlRequest {
                id: id.clone(),
                vars: (*self.request_vars).clone(),
                req,
            };

            let mut line = match serde_json::to_string(&request) {
                Ok(line) => line,
                Err(err) => {
                    warn!("Exec hook request serialization failed: {err}");
                    return original;
                }
            };
            line.push('\n');

            let response = match self
                .exchange_line(runtime, id.clone(), line, self.timeout)
                .await
            {
                Ok(response) => response,
                Err(()) => return original,
            };

            let mut response = response;
            if let Value::Object(ref mut obj) = response {
                obj.remove("id");
            }
            super::merge_hook_response(&original, response)
        }

        pub async fn startup(&self, vars: Value, timeout: Duration) -> Map<String, Value> {
            let runtime = match self.ensure_runtime().await {
                Ok(runtime) => runtime,
                Err(err) => {
                    warn!("Exec hook spawn failed during startup: {err}");
                    return Map::new();
                }
            };

            let id = format!("evt-{}", EVENT_COUNTER.fetch_add(1, Ordering::Relaxed));
            let request = StartupRequest {
                id: id.clone(),
                event: "startup",
                vars,
            };

            let mut line = match serde_json::to_string(&request) {
                Ok(line) => line,
                Err(err) => {
                    warn!("Exec hook startup request serialization failed: {err}");
                    return Map::new();
                }
            };
            line.push('\n');

            let response = match self.exchange_line(runtime, id.clone(), line, timeout).await {
                Ok(response) => response,
                Err(()) => return Map::new(),
            };

            extract_startup_env(&id, response)
        }

        async fn exchange_line(
            &self,
            runtime: RuntimeHandle,
            id: String,
            line: String,
            timeout: Duration,
        ) -> Result<Value, ()> {
            let (tx, rx) = oneshot::channel();
            runtime.pending.lock().await.insert(id.clone(), tx);

            let send_result = tokio::time::timeout(self.timeout, async {
                let mut stdin = runtime.stdin.lock().await;
                stdin.write_all(line.as_bytes()).await?;
                stdin.flush().await?;
                Ok::<(), std::io::Error>(())
            })
            .await;

            match send_result {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    runtime.pending.lock().await.remove(&id);
                    warn!("Exec hook write failed for `{id}`: {err}");
                    self.mark_dead().await;
                    return Err(());
                }
                Err(_) => {
                    runtime.pending.lock().await.remove(&id);
                    warn!(
                        "Exec hook write timed out for `{id}` after {:?}",
                        self.timeout
                    );
                    self.mark_dead().await;
                    return Err(());
                }
            }

            match tokio::time::timeout(timeout, rx).await {
                Ok(Ok(response)) => Ok(response),
                Ok(Err(_)) => {
                    runtime.pending.lock().await.remove(&id);
                    warn!("Exec hook exited before replying to `{id}`");
                    self.mark_dead().await;
                    Err(())
                }
                Err(_) => {
                    runtime.pending.lock().await.remove(&id);
                    warn!("Exec hook timed out for `{id}` after {:?}", timeout);
                    Err(())
                }
            }
        }

        async fn ensure_runtime(&self) -> Result<RuntimeHandle> {
            let mut state = self.state.lock().await;
            if state.is_none() {
                *state = Some(spawn_runtime(&self.source)?);
            }

            let runtime = state
                .as_ref()
                .ok_or_else(|| anyhow!("exec hook runtime missing after spawn"))?;

            Ok(RuntimeHandle {
                stdin: Arc::clone(&runtime.stdin),
                pending: Arc::clone(&runtime.pending),
            })
        }

        async fn mark_dead(&self) {
            let mut state = self.state.lock().await;
            if let Some(runtime) = state.take() {
                runtime.pending.lock().await.clear();
            }
        }
    }

    pub(crate) fn extract_startup_env(id: &str, response: Value) -> Map<String, Value> {
        let Value::Object(mut response_obj) = response else {
            warn!("Exec hook startup response for `{id}` was not an object");
            return Map::new();
        };

        let Some(env_value) = response_obj.remove("env") else {
            return Map::new();
        };

        let Value::Object(env_obj) = env_value else {
            warn!("Exec hook startup response for `{id}` had non-object `env`");
            return Map::new();
        };

        let mut env = Map::new();
        for (key, value) in env_obj {
            if matches!(value, Value::String(_)) {
                env.insert(key, value);
            }
        }
        env
    }

    fn validate_exec_path(path: &Path) -> Result<()> {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let metadata = fs::metadata(path)
            .with_context(|| format!("--hook path {} not found", path.display()))?;

        if !metadata.is_file() {
            bail!("--hook path {} is not a file", path.display());
        }

        if metadata.permissions().mode() & 0o111 == 0 {
            bail!("--hook path {} is not executable", path.display());
        }

        Ok(())
    }

    fn spawn_runtime(source: &ExecSource) -> Result<ProcessRuntime> {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        use std::process::Stdio;

        let (program, script_dir) = match source {
            ExecSource::Inline(script) => {
                let script_dir = Builder::new()
                    .prefix("harnx-hook-exec-")
                    .tempdir()
                    .context("create exec hook tempdir")?;
                let path = script_dir.path().join("hook");
                fs::write(&path, script.as_ref())
                    .with_context(|| format!("write exec hook script {}", path.display()))?;

                let mut permissions = fs::metadata(&path)
                    .with_context(|| format!("stat exec hook script {}", path.display()))?
                    .permissions();
                permissions.set_mode(0o755);
                fs::set_permissions(&path, permissions)
                    .with_context(|| format!("chmod exec hook script {}", path.display()))?;
                (path, Some(script_dir))
            }
            ExecSource::Path(path) => (path.clone(), None),
        };

        debug!(program = %program.display(), "Spawning resident exec hook process");
        let mut child = Command::new(&program)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawn exec hook {}", program.display()))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("missing exec hook stdin pipe"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("missing exec hook stdout pipe"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("missing exec hook stderr pipe"))?;

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let ready_seen = Arc::new(AtomicBool::new(false));

        let reader_pending = Arc::clone(&pending);
        let reader_ready_seen = Arc::clone(&ready_seen);
        let reader_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            let mut waiting_for_first_payload = true;

            loop {
                if waiting_for_first_payload {
                    match tokio::time::timeout(STARTUP_READY_TIMEOUT, lines.next_line()).await {
                        Ok(Ok(Some(line))) => {
                            let trimmed = line.trim();
                            if trimmed.is_empty() {
                                continue;
                            }
                            if trimmed == "READY" {
                                reader_ready_seen.store(true, Ordering::Relaxed);
                                debug!("Exec hook reported READY");
                                continue;
                            }

                            if resolve_exec_line(trimmed, &reader_pending).await {
                                waiting_for_first_payload = false;
                            }
                            continue;
                        }
                        Ok(Ok(None)) => {
                            debug!("Exec hook stdout reached EOF before READY");
                            break;
                        }
                        Ok(Err(err)) => {
                            warn!("Failed reading exec hook stdout before READY: {err}");
                            break;
                        }
                        Err(_) => {
                            debug!("Exec hook startup timed out waiting for READY; continuing");
                            waiting_for_first_payload = false;
                        }
                    }
                }

                match lines.next_line().await {
                    Ok(Some(line)) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        if trimmed == "READY" {
                            reader_ready_seen.store(true, Ordering::Relaxed);
                            debug!("Exec hook reported READY");
                            continue;
                        }

                        if resolve_exec_line(trimmed, &reader_pending).await {
                            waiting_for_first_payload = false;
                        }
                    }
                    Ok(None) => {
                        if waiting_for_first_payload && !reader_ready_seen.load(Ordering::Relaxed) {
                            debug!("Exec hook stdout reached EOF before READY");
                        } else {
                            debug!("Exec hook stdout reached EOF");
                        }
                        break;
                    }
                    Err(err) => {
                        if waiting_for_first_payload && !reader_ready_seen.load(Ordering::Relaxed) {
                            warn!("Failed reading exec hook stdout before READY: {err}");
                        } else {
                            warn!("Failed reading exec hook stdout: {err}");
                        }
                        break;
                    }
                }
            }

            reader_pending.lock().await.clear();
        });

        let stderr_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();

            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        let line = line.trim();
                        if !line.is_empty() {
                            warn!("Exec hook stderr: {line}");
                        }
                    }
                    Ok(None) => break,
                    Err(err) => {
                        warn!("Failed reading exec hook stderr: {err}");
                        break;
                    }
                }
            }
        });

        Ok(ProcessRuntime {
            _child: child,
            stdin: Arc::new(Mutex::new(stdin)),
            pending,
            _reader_task: reader_task,
            _stderr_task: stderr_task,
            _script_dir: script_dir,
        })
    }
}

#[cfg(not(unix))]
pub struct ExecHookProcess;

#[cfg(not(unix))]
impl ExecHookProcess {
    pub fn spawn_inline(_script: &str, _timeout_secs: u64) -> Result<Self> {
        Err(anyhow!("exec hook processes are only supported on unix"))
    }

    pub fn spawn_path(_path: PathBuf, _timeout_secs: u64) -> Result<Self> {
        Err(anyhow!("exec hook processes are only supported on unix"))
    }

    pub async fn transform(&self, req: Value) -> Value {
        req
    }

    pub async fn startup(&self, _vars: Value, _timeout: std::time::Duration) -> Map<String, Value> {
        Map::new()
    }

    pub fn with_request_vars(self, _request_vars: std::sync::Arc<Value>) -> Self {
        self
    }
}

#[cfg(unix)]
pub use imp::ExecHookProcess;

#[cfg(unix)]
fn merge_hook_response(original: &Value, response: Value) -> Value {
    let Value::Object(mut response_obj) = response else {
        return original.clone();
    };

    if crate::transform::should_short_circuit(&Value::Object(response_obj.clone())) {
        return Value::Object(response_obj);
    }

    let mut merged = match original {
        Value::Object(object) => object.clone(),
        _ => return original.clone(),
    };

    if let Some(Value::Object(response_headers)) = response_obj.remove("headers") {
        let mut merged_headers = match merged.remove("headers") {
            Some(Value::Object(headers)) => headers,
            _ => Map::new(),
        };
        merge_headers(&mut merged_headers, response_headers);
        merged.insert("headers".to_owned(), Value::Object(merged_headers));
    }

    for (key, value) in response_obj {
        merged.insert(key, value);
    }

    Value::Object(merged)
}

#[cfg(unix)]
fn merge_headers(original_headers: &mut Map<String, Value>, response_headers: Map<String, Value>) {
    // `request_json` lowercases all incoming header names, so normalize the
    // hook's header keys to lowercase too. This avoids a mixed-case key from a
    // custom script producing a duplicate entry (e.g. both `Authorization` and
    // `authorization`) whose downstream patch order would be ambiguous.
    for (key, value) in response_headers {
        let key = key.to_ascii_lowercase();
        match value {
            Value::String(_) => {
                original_headers.insert(key, value);
            }
            Value::Null => {
                original_headers.remove(&key);
            }
            _ => {}
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;

    #[cfg(unix)]
    fn python_startup_script(mode: &str) -> String {
        format!(
            "#!/usr/bin/env python3\nimport json, sys, time\nprint(\"READY\", flush=True)\nfor line in sys.stdin:\n    line = line.strip()\n    if not line:\n        continue\n    msg = json.loads(line)\n    req_id = msg[\"id\"]\n    if msg.get(\"event\") == \"startup\":\n        if \"{mode}\" == \"timeout\":\n            time.sleep(2.0)\n            continue\n        print(json.dumps({{\"id\": req_id, \"env\": {{\"K\": \"v\"}}}}), flush=True)\n        continue\n    msg[\"id\"] = req_id\n    print(json.dumps(msg), flush=True)\n"
        )
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_returns_env_map_from_hook_response() {
        let process = ExecHookProcess::spawn_inline(&python_startup_script("success"), 1).unwrap();

        let env = process
            .startup(
                json!({"proxy_port": 1234}),
                std::time::Duration::from_secs(1),
            )
            .await;

        assert_eq!(
            env,
            serde_json::Map::from_iter([(String::from("K"), json!("v"))])
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_timeout_returns_empty_map() {
        let process = ExecHookProcess::spawn_inline(&python_startup_script("timeout"), 1).unwrap();

        let env = process
            .startup(
                json!({"proxy_port": 1234}),
                std::time::Duration::from_millis(100),
            )
            .await;

        assert!(env.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn extract_startup_env_keeps_only_string_values() {
        let env = imp::extract_startup_env(
            "evt-1",
            json!({"env": {"KEEP": "ok", "DROP_NUM": 5, "DROP_BOOL": true}}),
        );

        assert_eq!(
            env,
            serde_json::Map::from_iter([(String::from("KEEP"), json!("ok"))])
        );
    }

    #[cfg(unix)]
    #[test]
    fn extract_startup_env_returns_empty_when_env_missing() {
        let env = imp::extract_startup_env("evt-1", json!({"id": "evt-1"}));

        assert!(env.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn extract_startup_env_returns_empty_when_env_is_not_object() {
        let env = imp::extract_startup_env("evt-1", json!({"env": ["bad"]}));

        assert!(env.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn sparse_exec_response_merges_with_original_request() {
        let original = json!({
            "method": "GET",
            "host": "example.com",
            "path": "/start",
            "headers": {"accept": "application/json", "x-old": "keep"}
        });
        let response = json!({ "headers": {"x": "1"} });

        let merged = merge_hook_response(&original, response);

        assert_eq!(merged["method"], "GET");
        assert_eq!(merged["host"], "example.com");
        assert_eq!(merged["path"], "/start");
        assert_eq!(merged["headers"]["accept"], "application/json");
        assert_eq!(merged["headers"]["x-old"], "keep");
        assert_eq!(merged["headers"]["x"], "1");
    }

    #[cfg(unix)]
    #[test]
    fn respond_exec_response_bypasses_merge() {
        let original = json!({
            "method": "GET",
            "host": "example.com",
            "path": "/start",
            "headers": {"accept": "application/json"}
        });
        let response = json!({ "respond": {"status": 202, "body": "ok"} });

        let merged = merge_hook_response(&original, response.clone());

        assert_eq!(merged, response);
    }

    #[cfg(unix)]
    #[test]
    fn header_null_removes_existing_key() {
        let original = json!({
            "method": "GET",
            "host": "example.com",
            "path": "/start",
            "headers": {"accept": "application/json", "x-remove": "gone"}
        });
        let response = json!({ "headers": {"x-remove": null, "x-keep": 5, "x-add": "1"} });

        let merged = merge_hook_response(&original, response);

        assert!(merged["headers"].get("x-remove").is_none());
        assert!(merged["headers"].get("x-keep").is_none());
        assert_eq!(merged["headers"]["x-add"], "1");
        assert_eq!(merged["headers"]["accept"], "application/json");
    }

    #[cfg(unix)]
    #[test]
    fn mixed_case_hook_header_key_is_lowercased_and_replaces_original() {
        let original = json!({
            "method": "GET",
            "host": "bigquery.googleapis.com",
            "path": "/",
            "headers": {"authorization": "Bearer old"}
        });
        let response = json!({ "headers": {"Authorization": "Bearer new"} });

        let merged = merge_hook_response(&original, response);

        let headers = merged["headers"].as_object().expect("headers object");
        assert_eq!(headers["authorization"], "Bearer new");
        assert!(headers.get("Authorization").is_none());
        assert_eq!(headers.len(), 1);
    }
}
