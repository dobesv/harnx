use crate::{
    executor::control_from_result, HookCommand, HookOutcome, HookPayload, HookResult,
    HookResultControl,
};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex as StdMutex,
};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{oneshot, Mutex};

static EVENT_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Suppress repeated failure notices for the same hook command within this
/// window, so a hook that fails on every tool call doesn't spam the UI.
const HOOK_ERROR_NOTICE_WINDOW: Duration = Duration::from_secs(30);

/// Max stderr lines retained per persistent hook, surfaced in a failure notice.
const HOOK_STDERR_TAIL_LINES: usize = 20;

#[derive(Serialize)]
struct JsonlRequest<'a> {
    id: String,
    #[serde(flatten)]
    payload: &'a HookPayload,
}

#[derive(Deserialize)]
struct JsonlResponse {
    id: String,
    #[serde(flatten)]
    result: HookResult,
}

pub struct PersistentHookProcess {
    _child: Child,
    stdin: ChildStdin,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<HookResult>>>>,
    _reader_task: tokio::task::JoinHandle<()>,
    _stderr_task: tokio::task::JoinHandle<()>,
    /// PID of the hook child process, captured at spawn. For diagnostics.
    pid: Option<u32>,
    /// Recent stderr lines (bounded), used to explain failures in a notice.
    stderr_lines: Arc<StdMutex<VecDeque<String>>>,
}

impl PersistentHookProcess {
    /// Snapshot the retained stderr tail (joined by newlines).
    fn stderr_tail(&self) -> String {
        self.stderr_lines
            .lock()
            .map(|buf| buf.iter().cloned().collect::<Vec<_>>().join("\n"))
            .unwrap_or_default()
    }
}

pub struct PersistentHookManager {
    processes: HashMap<String, PersistentHookProcess>,
    /// Last time a failure notice was emitted per hook command, for dedup.
    last_error_notice: HashMap<String, Instant>,
}

impl PersistentHookManager {
    pub fn new() -> Self {
        Self {
            processes: HashMap::new(),
            last_error_notice: HashMap::new(),
        }
    }

    pub async fn send_event(&mut self, payload: &HookPayload, hook: &HookCommand) -> HookOutcome {
        let command = hook.command.as_str();
        if !self.processes.contains_key(command) {
            match PersistentHookProcess::spawn(hook) {
                Ok(process) => {
                    self.processes.insert(command.to_string(), process);
                }
                Err(err) => {
                    warn!("Failed to spawn persistent hook `{command}`: {err}");
                    self.notify_hook_failure(command, &format!("failed to launch: {err}"));
                    return continue_with_default();
                }
            }
        }

        // Scope the process borrow so the error path can touch `self` again.
        let result = {
            let process = self
                .processes
                .get_mut(command)
                .expect("persistent hook process inserted before use");
            process.send_event(payload, hook.timeout).await
        };

        match result {
            Ok(outcome) => {
                // Recovered — allow a fresh notice if it fails again later.
                self.last_error_notice.remove(command);
                outcome
            }
            Err(err) => {
                // Give the stderr reader a moment to drain the child's final
                // output so the failure notice can include the real error.
                tokio::time::sleep(Duration::from_millis(50)).await;
                let stderr_tail = self
                    .processes
                    .get(command)
                    .map(PersistentHookProcess::stderr_tail)
                    .unwrap_or_default();
                self.processes.remove(command);
                warn!("Persistent hook `{command}` failed: {err}, removing process");
                let detail = if stderr_tail.trim().is_empty() {
                    format!("exited unexpectedly ({err}) — its hook actions are not being applied")
                } else {
                    format!("exited on startup — its hook actions are not being applied:\n{stderr_tail}")
                };
                self.notify_hook_failure(command, &detail);
                continue_with_default()
            }
        }
    }

    /// Surface a persistent-hook failure to the active UI via the process-wide
    /// agent-event sink, deduped per command within [`HOOK_ERROR_NOTICE_WINDOW`]
    /// so a hook that fails every tool call doesn't spam. Falls back to the log
    /// when no sink is installed.
    fn notify_hook_failure(&mut self, command: &str, detail: &str) {
        let now = Instant::now();
        if let Some(prev) = self.last_error_notice.get(command) {
            if now.duration_since(*prev) < HOOK_ERROR_NOTICE_WINDOW {
                return;
            }
        }
        self.last_error_notice.insert(command.to_string(), now);

        let program = command.split_whitespace().next().unwrap_or(command);
        let message = format!("Hook `{program}` {detail}");
        let emitted = harnx_core::sink::emit_agent_event(harnx_core::event::AgentEvent::Notice(
            harnx_core::event::NoticeEvent::Error(message.clone()),
        ));
        if !emitted {
            warn!("{message}");
        }
    }

    pub fn shutdown(&mut self) {
        self.processes.clear();
    }

    /// PID of the running persistent-hook process for `command`, if one is
    /// currently spawned. Keyed by the exact hook command string. For
    /// diagnostics (`.info mcp`).
    pub fn pid_for(&self, command: &str) -> Option<u32> {
        self.processes.get(command).and_then(|process| process.pid)
    }
}

impl Default for PersistentHookManager {
    fn default() -> Self {
        Self::new()
    }
}

impl PersistentHookProcess {
    fn spawn(hook: &HookCommand) -> Result<Self> {
        let mut child =
            super::executor::base_hook_command(&hook.command, hook.package_dir.as_deref())
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn()?;

        let pid = child.id();

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("missing stdin pipe"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("missing stdout pipe"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("missing stderr pipe"))?;

        let pending: Arc<Mutex<HashMap<String, oneshot::Sender<HookResult>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let reader_pending = Arc::clone(&pending);
        let reader_task = tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();

            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        // A standalone {"notice": {...}} line surfaces a message
                        // to the UI; it carries no event id, so it is not a
                        // response to a pending request.
                        if emit_hook_notice_line(&line) {
                            continue;
                        }
                        match serde_json::from_str::<JsonlResponse>(&line) {
                            Ok(response) => {
                                let mut map = reader_pending.lock().await;
                                if let Some(sender) = map.remove(&response.id) {
                                    let _ = sender.send(response.result);
                                } else {
                                    warn!(
                                        "Persistent hook returned response for unknown event id `{}`",
                                        response.id
                                    );
                                }
                            }
                            Err(err) => {
                                warn!("Failed to parse persistent hook response: {err}");
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(err) => {
                        warn!("Failed reading persistent hook stdout: {err}");
                        break;
                    }
                }
            }

            reader_pending.lock().await.clear();
        });

        let stderr_lines: Arc<StdMutex<VecDeque<String>>> =
            Arc::new(StdMutex::new(VecDeque::new()));
        let stderr_lines_task = Arc::clone(&stderr_lines);
        let stderr_task = tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();

            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        let line = line.trim();
                        if !line.is_empty() {
                            warn!("Persistent hook stderr: {line}");
                            if let Ok(mut buf) = stderr_lines_task.lock() {
                                if buf.len() == HOOK_STDERR_TAIL_LINES {
                                    buf.pop_front();
                                }
                                buf.push_back(line.to_string());
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(err) => {
                        warn!("Failed reading persistent hook stderr: {err}");
                        break;
                    }
                }
            }
        });

        Ok(Self {
            _child: child,
            stdin,
            pending,
            _reader_task: reader_task,
            _stderr_task: stderr_task,
            pid,
            stderr_lines,
        })
    }

    async fn send_event(
        &mut self,
        payload: &HookPayload,
        timeout_secs: Option<u64>,
    ) -> Result<HookOutcome> {
        let id = format!("evt-{}", EVENT_COUNTER.fetch_add(1, Ordering::Relaxed));
        let request = JsonlRequest {
            id: id.clone(),
            payload,
        };

        let mut line = serde_json::to_string(&request)?;
        line.push('\n');

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), tx);

        if let Err(err) = self.stdin.write_all(line.as_bytes()).await {
            self.pending.lock().await.remove(&id);
            return Err(err.into());
        }

        if let Err(err) = self.stdin.flush().await {
            self.pending.lock().await.remove(&id);
            return Err(err.into());
        }

        let timeout = Duration::from_secs(timeout_secs.unwrap_or(30));
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => {
                let control = control_from_result(&result);
                Ok(HookOutcome { control, result })
            }
            Ok(Err(_)) => {
                self.pending.lock().await.remove(&id);
                bail!("persistent hook process exited unexpectedly")
            }
            Err(_) => {
                self.pending.lock().await.remove(&id);
                warn!("Persistent hook timed out for event `{id}`");
                Ok(continue_with_default())
            }
        }
    }
}

fn continue_with_default() -> HookOutcome {
    HookOutcome {
        control: HookResultControl::Continue,
        result: HookResult::default(),
    }
}

/// If `line` is a standalone hook-notice message
/// (`{"notice": {"level": …, "message": …}}`), emit it to the user through the
/// agent-event sink and return `true`. Level `error` → Error notice, `info` →
/// Info, anything else → Warning.
fn emit_hook_notice_line(line: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return false;
    };
    let Some(notice) = value.get("notice").and_then(Value::as_object) else {
        return false;
    };
    let message = notice
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if message.is_empty() {
        return false;
    }
    let level = notice
        .get("level")
        .and_then(Value::as_str)
        .unwrap_or("warning");
    let event = match level {
        "error" => harnx_core::event::NoticeEvent::Error(message.to_string()),
        "info" => harnx_core::event::NoticeEvent::Info(message.to_string()),
        _ => harnx_core::event::NoticeEvent::Warning(message.to_string()),
    };
    harnx_core::sink::emit_agent_event(harnx_core::event::AgentEvent::Notice(event));
    true
}

#[cfg(test)]
mod tests {
    use super::{PersistentHookManager, PersistentHookProcess};
    use crate::{HookCommand, HookEvent, HookPayload, HookResultControl};

    fn hook_cmd(command: String, timeout: Option<u64>) -> HookCommand {
        HookCommand {
            command,
            timeout,
            package_dir: None,
        }
    }
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    #[cfg(unix)]
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[cfg(unix)]
    static SCRIPT_COUNTER: AtomicU64 = AtomicU64::new(1);

    fn test_payload(cwd: &Path) -> HookPayload {
        HookPayload {
            session_id: "session-123".to_string(),
            cwd: cwd.to_path_buf(),
            resume_count: 0,
            hook_event: HookEvent::Stop {
                stop_hook_active: false,
                last_assistant_message: Some("done".to_string()),
            },
        }
    }

    fn temp_test_dir(name: &str) -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("unix epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("harnx-persistent-hook-tests-{name}-{suffix}"));
        fs::create_dir_all(&dir).expect("create temp test dir");
        dir
    }

    #[cfg(unix)]
    fn shell_quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', r#"'\''"#))
    }

    #[cfg(windows)]
    fn powershell_quote(value: &str) -> String {
        value.replace('\'', "''")
    }

    #[cfg(windows)]
    fn encode_powershell_script(script: &str) -> String {
        let utf16: Vec<u8> = script
            .encode_utf16()
            .flat_map(|unit| unit.to_le_bytes())
            .collect();
        harnx_core::crypto::base64_encode(utf16)
    }

    #[cfg(unix)]
    fn write_script(dir: &Path, name: &str, body: &str) -> String {
        let id = SCRIPT_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!("{name}-{id}.sh"));
        fs::write(&path, format!("#!/bin/sh\nset -eu\n{body}\n")).expect("write shell script");

        let mut permissions = fs::metadata(&path).expect("script metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).expect("set shell script permissions");

        shell_quote(&path.display().to_string())
    }

    #[cfg(windows)]
    fn write_script(_dir: &Path, _name: &str, body: &str) -> String {
        let wrapped = format!("$ProgressPreference = 'SilentlyContinue'\n{body}");
        let encoded = encode_powershell_script(&wrapped);
        format!("powershell.exe -NoProfile -ExecutionPolicy Bypass -EncodedCommand {encoded}")
    }

    #[cfg(unix)]
    fn extract_id_snippet() -> &'static str {
        r#"id=${line#*\"id\":\"}; id=${id%%\"*}"#
    }

    #[cfg(unix)]
    fn respond_command(dir: &Path, marker: Option<&Path>, additional_context: &str) -> String {
        let startup = marker
            .map(|path| {
                format!(
                    "printf '%s\\n' {} >> {}\n",
                    shell_quote("spawned"),
                    shell_quote(&path.display().to_string())
                )
            })
            .unwrap_or_default();

        write_script(
            dir,
            "respond",
            &format!(
                "{startup}while IFS= read -r line; do {}; printf '{{\"id\":\"%s\",\"additionalContext\":\"{}\"}}\\n' \"$id\"; done",
                extract_id_snippet(),
                additional_context.replace('"', "\\\"")
            ),
        )
    }

    #[cfg(windows)]
    fn respond_command(dir: &Path, marker: Option<&Path>, additional_context: &str) -> String {
        let startup = marker
            .map(|path| {
                format!(
                    "Add-Content -Path '{}' -Value 'spawned'\n",
                    powershell_quote(&path.display().to_string())
                )
            })
            .unwrap_or_default();

        write_script(
            dir,
            "respond",
            &format!(
                "{startup}while (($line = [Console]::In.ReadLine()) -ne $null) {{\n    if ($line -match '\"id\":\"([^\"]+)\"') {{\n        $id = $Matches[1]\n        $output = @{{ id = $id; additionalContext = '{}' }} | ConvertTo-Json -Compress\n        [Console]::Out.WriteLine($output)\n    }}\n}}\n",
                powershell_quote(additional_context)
            ),
        )
    }

    #[cfg(unix)]
    fn timeout_command(dir: &Path) -> String {
        write_script(
            dir,
            "timeout",
            "while IFS= read -r _line; do sleep 60; done",
        )
    }

    #[cfg(windows)]
    fn timeout_command(dir: &Path) -> String {
        write_script(
            dir,
            "timeout",
            "while (($line = [Console]::In.ReadLine()) -ne $null) {\n    Start-Sleep -Seconds 60\n}\n",
        )
    }

    #[tokio::test]
    async fn test_persistent_process_send_and_receive() {
        let cwd = temp_test_dir("send-and-receive");
        let payload = test_payload(&cwd);
        let mut process = PersistentHookProcess::spawn(&hook_cmd(
            respond_command(&cwd, None, "persistent response"),
            None,
        ))
        .expect("spawn persistent hook");

        let outcome = process
            .send_event(&payload, Some(5))
            .await
            .expect("send event");

        assert!(matches!(outcome.control, HookResultControl::Continue));
        assert_eq!(
            outcome.result.additional_context.as_deref(),
            Some("persistent response")
        );
    }

    #[tokio::test]
    async fn test_persistent_manager_reuses_process() {
        let cwd = temp_test_dir("reuse-process");
        let marker = cwd.join("persistent-spawns.txt");
        let payload = test_payload(&cwd);
        let command = respond_command(&cwd, Some(&marker), "persistent response");
        let mut manager = PersistentHookManager::new();

        let hook = hook_cmd(command.clone(), Some(5));
        let first = manager.send_event(&payload, &hook).await;
        let second = manager.send_event(&payload, &hook).await;

        assert!(matches!(first.control, HookResultControl::Continue));
        assert!(matches!(second.control, HookResultControl::Continue));

        let contents = fs::read_to_string(&marker).expect("read spawn marker");
        assert_eq!(contents.lines().count(), 1);

        manager.shutdown();
    }

    #[tokio::test]
    async fn test_persistent_process_timeout() {
        let cwd = temp_test_dir("timeout");
        let payload = test_payload(&cwd);
        let mut process = PersistentHookProcess::spawn(&hook_cmd(timeout_command(&cwd), None))
            .expect("spawn timeout hook");
        let start = tokio::time::Instant::now();

        let outcome = process
            .send_event(&payload, Some(1))
            .await
            .expect("timeout should continue");

        assert!(matches!(outcome.control, HookResultControl::Continue));
        assert_eq!(outcome.result.additional_context, None);
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    /// A persistent hook that crashes should degrade to Continue AND surface an
    /// error notice (including the child's stderr) through the agent-event sink.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_failing_persistent_hook_surfaces_notice() {
        use harnx_core::event::{AgentEvent, AgentEventSink, AgentSource, NoticeEvent};
        use std::sync::{Arc, Mutex};

        #[derive(Default)]
        struct CollectingSink {
            events: Mutex<Vec<AgentEvent>>,
        }
        impl AgentEventSink for CollectingSink {
            fn emit(&self, event: AgentEvent, _source: Option<AgentSource>) {
                self.events.lock().unwrap().push(event);
            }
        }

        let sink = Arc::new(CollectingSink::default());
        harnx_core::sink::install_agent_event_sink(sink.clone());

        let cwd = temp_test_dir("failing-hook");
        let payload = test_payload(&cwd);
        let mut manager = PersistentHookManager::new();
        let hook = hook_cmd(
            "echo 'boom-marker: bad flag' >&2; sleep 0.1; exit 1".to_string(),
            Some(5),
        );

        let outcome = manager.send_event(&payload, &hook).await;
        harnx_core::sink::clear_agent_event_sink();

        // Failure must not block the tool — it degrades to Continue.
        assert!(matches!(outcome.control, HookResultControl::Continue));

        let events = sink.events.lock().unwrap();
        let notice = events.iter().find_map(|event| match event {
            AgentEvent::Notice(NoticeEvent::Error(msg)) => Some(msg.clone()),
            _ => None,
        });
        let msg = notice.expect("a failure notice should be emitted");
        assert!(msg.contains("exited"), "unexpected notice: {msg}");
        assert!(msg.contains("boom-marker"), "stderr not surfaced: {msg}");
    }

    /// A live hook that emits a `{"notice": {...}}` line on stdout surfaces it
    /// as a Notice while still answering the request normally.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_persistent_hook_stdout_notice_surfaces() {
        use harnx_core::event::{AgentEvent, AgentEventSink, AgentSource, NoticeEvent};
        use std::sync::{Arc, Mutex};

        #[derive(Default)]
        struct CollectingSink {
            events: Mutex<Vec<AgentEvent>>,
        }
        impl AgentEventSink for CollectingSink {
            fn emit(&self, event: AgentEvent, _source: Option<AgentSource>) {
                self.events.lock().unwrap().push(event);
            }
        }

        let sink = Arc::new(CollectingSink::default());
        harnx_core::sink::install_agent_event_sink(sink.clone());

        let cwd = temp_test_dir("stdout-notice");
        let payload = test_payload(&cwd);
        let mut manager = PersistentHookManager::new();
        // For each event: emit a notice line, then echo the event id as the
        // response so send_event completes normally.
        let cmd = r#"while IFS= read -r line; do printf '{"notice":{"level":"warning","message":"heads-up-42"}}\n'; id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p'); printf '{"id":"%s"}\n' "$id"; done"#;

        let outcome = manager
            .send_event(&payload, &hook_cmd(cmd.to_string(), Some(5)))
            .await;
        // Let the reader task drain the notice line.
        tokio::time::sleep(Duration::from_millis(100)).await;
        harnx_core::sink::clear_agent_event_sink();

        assert!(matches!(outcome.control, HookResultControl::Continue));
        let events = sink.events.lock().unwrap();
        let surfaced = events.iter().any(|event| {
            matches!(event, AgentEvent::Notice(NoticeEvent::Warning(msg)) if msg.contains("heads-up-42"))
        });
        assert!(surfaced, "stdout notice not surfaced: {:?}", *events);
    }
}
