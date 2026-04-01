#![allow(dead_code)]

use crate::hooks::{HookOutcome, HookPayload, HookResult, HookResultControl};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Mutex};

static EVENT_COUNTER: AtomicU64 = AtomicU64::new(1);

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
}

pub struct PersistentHookManager {
    processes: HashMap<String, PersistentHookProcess>,
}

impl PersistentHookManager {
    pub fn new() -> Self {
        Self {
            processes: HashMap::new(),
        }
    }

    pub async fn send_event(
        &mut self,
        command: &str,
        payload: &HookPayload,
        timeout_secs: Option<u64>,
    ) -> HookOutcome {
        if !self.processes.contains_key(command) {
            match PersistentHookProcess::spawn(command) {
                Ok(process) => {
                    self.processes.insert(command.to_string(), process);
                }
                Err(err) => {
                    warn!("Failed to spawn persistent hook `{command}`: {err}");
                    return continue_with_default();
                }
            }
        }

        let process = self
            .processes
            .get_mut(command)
            .expect("persistent hook process inserted before use");

        match process.send_event(payload, timeout_secs).await {
            Ok(outcome) => outcome,
            Err(err) => {
                warn!("Persistent hook `{command}` failed: {err}, removing process");
                self.processes.remove(command);
                continue_with_default()
            }
        }
    }

    pub fn shutdown(&mut self) {
        self.processes.clear();
    }
}

impl Default for PersistentHookManager {
    fn default() -> Self {
        Self::new()
    }
}

impl PersistentHookProcess {
    fn spawn(command: &str) -> Result<Self> {
        let shell = super::executor::default_shell();
        let shell_arg = super::executor::default_shell_arg();

        let mut child = Command::new(&shell)
            .arg(shell_arg)
            .arg(command)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;

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
                    Ok(Some(line)) => match serde_json::from_str::<JsonlResponse>(&line) {
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
                    },
                    Ok(None) => break,
                    Err(err) => {
                        warn!("Failed reading persistent hook stdout: {err}");
                        break;
                    }
                }
            }

            reader_pending.lock().await.clear();
        });

        let stderr_task = tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();

            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        let line = line.trim();
                        if !line.is_empty() {
                            warn!("Persistent hook stderr: {line}");
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
            Ok(Ok(result)) => Ok(HookOutcome {
                control: HookResultControl::Continue,
                result,
            }),
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

#[cfg(test)]
mod tests {
    use super::{PersistentHookManager, PersistentHookProcess};
    use crate::hooks::{HookEvent, HookPayload, HookResultControl};
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
        crate::utils::base64_encode(utf16)
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
        let mut process =
            PersistentHookProcess::spawn(&respond_command(&cwd, None, "persistent response"))
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

        let first = manager.send_event(&command, &payload, Some(5)).await;
        let second = manager.send_event(&command, &payload, Some(5)).await;

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
        let mut process =
            PersistentHookProcess::spawn(&timeout_command(&cwd)).expect("spawn timeout hook");
        let start = tokio::time::Instant::now();

        let outcome = process
            .send_event(&payload, Some(1))
            .await
            .expect("timeout should continue");

        assert!(matches!(outcome.control, HookResultControl::Continue));
        assert_eq!(outcome.result.additional_context, None);
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
