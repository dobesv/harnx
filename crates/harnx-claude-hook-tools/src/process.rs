use crate::{protocol, RunRequest, MAX_BYTES};
use anyhow::{anyhow, bail, ensure, Context, Result};
use harnx_core::child_process::ChildProcessManager;
use serde_json::Value;
use std::{collections::HashMap, path::PathBuf, process::Stdio, sync::Arc};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
    task::JoinHandle,
};

const MAX_PROCESSES: usize = 32;
const STDERR_BYTES: usize = 4096;

type Slot = Arc<Mutex<Option<Process>>>;

#[derive(Clone, Hash, PartialEq, Eq)]
struct ProcessKey {
    command: Vec<String>,
    cwd: PathBuf,
    package_dir: Option<PathBuf>,
    session_id: String,
}

/// Slots serialize each resident hook. Taking the process out while it runs
/// makes dropped/timed-out exchanges retire it instead of reusing a dirty pipe.
#[derive(Default)]
pub(crate) struct Pool {
    slots: Mutex<HashMap<ProcessKey, Slot>>,
    manager: ChildProcessManager,
}

impl Pool {
    async fn slot(&self, request: &RunRequest) -> Result<Slot> {
        let key = ProcessKey {
            command: request.command.clone(),
            cwd: request.cwd()?,
            package_dir: request.package_dir.clone(),
            session_id: request.payload["session_id"]
                .as_str()
                .unwrap_or_default()
                .into(),
        };
        let mut slots = self.slots.lock().await;
        if let Some(slot) = slots.get(&key) {
            return Ok(slot.clone());
        }
        if slots.len() == MAX_PROCESSES {
            let idle = slots
                .iter()
                .find(|(_, slot)| Arc::strong_count(slot) == 1)
                .map(|(key, _)| key.clone());
            match idle {
                Some(key) => {
                    slots.remove(&key);
                }
                None => bail!("all {MAX_PROCESSES} persistent hook slots are busy"),
            }
        }
        let slot = Slot::default();
        slots.insert(key, slot.clone());
        Ok(slot)
    }

    pub async fn run(&self, request: &RunRequest) -> Result<protocol::HookRunResult> {
        if !request.persistent {
            let mut process = Process::spawn(&self.manager, request).await?;
            return process.once(request).await;
        }
        let slot = self.slot(request).await?;
        let mut cached = slot.lock().await;
        let mut process = match cached.take() {
            Some(process) => process,
            None => Process::spawn(&self.manager, request).await?,
        };
        let result = process.exchange_with_diagnostics(request).await?;
        *cached = Some(process);
        Ok(result)
    }
}

struct Process {
    child: Child,
    manager: ChildProcessManager,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    stderr: Arc<Mutex<Vec<u8>>>,
    stderr_task: JoinHandle<()>,
}

impl Process {
    fn command(request: &RunRequest) -> Result<Command> {
        let mut command = Command::new(&request.command[0]);
        command
            .args(&request.command[1..])
            .current_dir(request.cwd()?)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Never let an inherited protocol mode turn a one-shot command into a
        // resident process. Persistent children explicitly opt in to JSONL.
        command.env_remove("HARNX_HOOK_PROTOCOL");
        if request.persistent {
            command.env("HARNX_HOOK_PROTOCOL", "jsonl");
        }
        if let Some(dir) = &request.package_dir {
            command.env("HARNX_PACKAGE_DIR", dir);
        }
        Ok(command)
    }

    async fn spawn(manager: &ChildProcessManager, request: &RunRequest) -> Result<Self> {
        let mut child = manager
            .spawn(Self::command(request)?)
            .await
            .context("spawn Claude hook")?;
        let stdin = required_pipe(child.stdin.take(), "stdin")?;
        let stdout = required_pipe(child.stdout.take(), "stdout")?;
        let stderr_pipe = required_pipe(child.stderr.take(), "stderr")?;
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let stderr_task = tokio::spawn(drain_stderr(stderr_pipe, stderr.clone()));
        Ok(Self {
            child,
            manager: manager.clone(),
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
            stderr,
            stderr_task,
        })
    }

    async fn once(&mut self, request: &RunRequest) -> Result<protocol::HookRunResult> {
        let bytes = serde_json::to_vec(&request.wire_payload())?;
        // Read while writing: large payloads and eager hook output must not
        // deadlock two full OS pipes.
        let mut stdin = self
            .stdin
            .take()
            .ok_or_else(|| anyhow!("hook stdin closed"))?;
        let write = async move {
            let result = ignore_broken_pipe(stdin.write_all(&bytes).await);
            // ChildStdin::shutdown doesn't close the pipe. EOF requires Drop.
            drop(stdin);
            result
        };
        let read = bounded_output(&mut self.stdout);
        let (_, stdout) = tokio::try_join!(write, read)?;
        let status = self.child.wait().await?;
        (&mut self.stderr_task).await?;
        match status.code() {
            Some(0) => protocol::parse_stdout(&stdout),
            Some(2) => Ok(protocol::HookRunResult::blocked(
                String::from_utf8_lossy(&self.stderr.lock().await)
                    .trim()
                    .to_string(),
            )),
            code => bail!(
                "Claude hook exited with {code:?}: {}",
                String::from_utf8_lossy(&self.stderr.lock().await).trim()
            ),
        }
    }

    async fn exchange_with_diagnostics(
        &mut self,
        request: &RunRequest,
    ) -> Result<protocol::HookRunResult> {
        let result = self.exchange(request).await;
        let stderr = self.stderr.lock().await;
        result.map_err(|error| {
            anyhow!(
                "{error:#}; hook stderr: {}",
                String::from_utf8_lossy(&stderr).trim()
            )
        })
    }

    async fn exchange(&mut self, request: &RunRequest) -> Result<protocol::HookRunResult> {
        let id = uuid::Uuid::new_v4().to_string();
        let mut payload = request.wire_payload();
        payload["id"] = Value::String(id.clone());
        let mut bytes = serde_json::to_vec(&payload)?;
        bytes.push(b'\n');
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("hook stdin closed"))?;
        let write = async {
            stdin.write_all(&bytes).await?;
            stdin.flush().await?;
            Ok::<_, anyhow::Error>(())
        };
        let (_, response) = tokio::try_join!(write, Self::read_response(&mut self.stdout, &id))?;
        Ok(response)
    }

    async fn read_response(
        stdout: &mut BufReader<ChildStdout>,
        id: &str,
    ) -> Result<protocol::HookRunResult> {
        // Resident helpers may emit standalone notices before their response.
        // Bound their aggregate bytes as well as each line.
        let mut remaining = MAX_BYTES;
        loop {
            let line = bounded_line(stdout, remaining).await?;
            remaining -= line.len();
            let mut response: Value =
                serde_json::from_slice(&line).context("invalid hook JSONL response")?;
            ensure!(
                response.is_object(),
                "hook JSONL response must be an object"
            );
            if response.get("notice").is_some() && response.get("id").is_none() {
                continue;
            }
            ensure!(
                response["id"].as_str() == Some(id),
                "hook JSONL response id does not match request"
            );
            response
                .as_object_mut()
                .ok_or_else(|| anyhow!("invalid hook response"))?
                .remove("id");
            return protocol::parse_result(response);
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.manager.stop_process_group(&self.child);
        let _ = self.child.start_kill();
        self.stderr_task.abort();
    }
}

fn required_pipe<T>(pipe: Option<T>, name: &str) -> Result<T> {
    pipe.ok_or_else(|| anyhow!("missing hook {name}"))
}

fn ignore_broken_pipe(result: std::io::Result<()>) -> Result<()> {
    match result {
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        result => Ok(result?),
    }
}

async fn bounded_output(reader: &mut (impl AsyncRead + Unpin)) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take((MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .await?;
    ensure!(
        bytes.len() <= MAX_BYTES,
        "hook stdout exceeds {MAX_BYTES} bytes"
    );
    Ok(bytes)
}

async fn bounded_line(reader: &mut BufReader<ChildStdout>, limit: usize) -> Result<Vec<u8>> {
    let mut line = Vec::new();
    loop {
        let buffer = reader.fill_buf().await?;
        ensure!(
            !buffer.is_empty(),
            "persistent hook closed stdout before replying"
        );
        let end = buffer.iter().position(|byte| *byte == b'\n').map(|i| i + 1);
        let count = end.unwrap_or(buffer.len());
        ensure!(
            line.len() + count <= limit,
            "hook JSONL output exceeds {MAX_BYTES} bytes"
        );
        line.extend_from_slice(&buffer[..count]);
        reader.consume(count);
        if end.is_some() {
            return Ok(line);
        }
    }
}

async fn drain_stderr(mut reader: impl AsyncRead + Unpin, tail: Arc<Mutex<Vec<u8>>>) {
    let mut buffer = [0; STDERR_BYTES];
    while let Ok(count) = reader.read(&mut buffer).await {
        if count == 0 {
            break;
        }
        let mut tail = tail.lock().await;
        tail.extend_from_slice(&buffer[..count]);
        let excess = tail.len().saturating_sub(STDERR_BYTES);
        tail.drain(..excess);
    }
}
