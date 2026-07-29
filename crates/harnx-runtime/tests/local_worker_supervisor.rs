// The local worker supervisor spawns worker subprocesses and manages them via
// Unix process groups / signals (see `local_orchestrator`), so these
// integration tests are Unix-only. On non-Unix targets the file compiles to
// nothing, avoiding unused-import/dead-code errors under `-D warnings`.
#![cfg(unix)]

#[allow(dead_code)]
mod common;

use harnx_core::{event::NullSink, require_nextest, session::SessionLogEntry};
use harnx_runtime::config::LOCAL_CLUSTER_KEY;
use harnx_runtime::local_orchestrator::{local_worker_lock_file, LocalWorkerSupervisor};
use harnx_runtime::nats_session_log::NatsSessionLog;
use harnx_runtime::nats_worker::worker_ready_subject;
use harnx_runtime::{ThinClientConfig, ThinClientSession};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct EnvGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &Path) -> Self {
        let previous = std::env::var_os(key);
        // Nextest runs this test in its own process.
        unsafe { std::env::set_var(key, value) };
        Self { key, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => unsafe { std::env::set_var(self.key, value) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

fn nats_server_available() -> bool {
    std::env::var_os("NATS_SERVER_BIN")
        .map(PathBuf::from)
        .is_some_and(|path| path.is_file())
        || which::which("nats-server").is_ok()
}

fn skip_without_binaries() -> Option<PathBuf> {
    if !nats_server_available() {
        eprintln!("skipping local worker supervisor test: nats-server binary not found");
        return None;
    }
    let Some(binary) = common::harnx_binary() else {
        eprintln!("skipping local worker supervisor test: harnx binary not found");
        return None;
    };
    Some(binary)
}

fn isolated_environment(root: &Path) -> Vec<EnvGuard> {
    let config = root.join("config");
    let data = root.join("data");
    let state = root.join("state");
    std::fs::create_dir_all(&config).expect("create config directory");
    std::fs::create_dir_all(&data).expect("create data directory");
    std::fs::create_dir_all(&state).expect("create state directory");
    vec![
        EnvGuard::set("HARNX_CONFIG_DIR", &config),
        EnvGuard::set("HARNX_DATA_DIR", &data),
        EnvGuard::set("HARNX_STATE_DIR", &state),
    ]
}

async fn start_mock_openai() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock OpenAI server");
    let address = listener.local_addr().expect("mock OpenAI address");
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept OpenAI request");
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let read = socket.read(&mut buffer).await.expect("read OpenAI request");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
            if let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if request.len() >= header_end + 4 + content_length {
                    break;
                }
            }
        }
        let body = r#"{"id":"chatcmpl-local","object":"chat.completion","created":1,"model":"test","choices":[{"index":0,"message":{"role":"assistant","content":"worker completed"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write OpenAI response");
    });
    (format!("http://{address}/v1"), task)
}

fn write_trivial_agent_config(root: &Path, api_base: &str) {
    let clients = root.join("config/clients");
    let agents = root.join("config/agents");
    std::fs::create_dir_all(&clients).expect("create clients directory");
    std::fs::create_dir_all(&agents).expect("create agents directory");
    std::fs::write(
        root.join("config/config.yaml"),
        "save: false\nstream: false\nclient: mock\nmodel: mock:test\n",
    )
    .expect("write global config");
    std::fs::write(
        clients.join("mock.yaml"),
        format!(
            "type: openai-compatible\nname: mock\napi_base: {api_base:?}\napi_key: test-key\nmodels:\n  - name: test\n    max_input_tokens: 32000\n    max_output_tokens: 1024\n"
        ),
    )
    .expect("write mock client config");
    std::fs::write(
        agents.join("trivial.md"),
        "---\nmodel: mock:test\nstream: false\n---\nComplete the turn.\n",
    )
    .expect("write trivial agent");
}

async fn assert_worker_completes_turn(
    supervisor: &LocalWorkerSupervisor,
    mock_task: tokio::task::JoinHandle<()>,
) {
    let client = async_nats::ConnectOptions::new()
        .token(supervisor.server().token.clone())
        .connect(&supervisor.server().url)
        .await
        .expect("connect thin client");
    let jetstream = async_nats::jetstream::new(client.clone());
    let session_id = format!("local-supervisor-{}", uuid::Uuid::new_v4());
    let log = NatsSessionLog::new(jetstream.clone(), session_id.clone());
    log.append_event_async(&SessionLogEntry::Header {
        model_id: "mock:test".to_string(),
        temperature: None,
        top_p: None,
        use_tools: Some(vec![]),
        save_session: Some(false),
        compress_threshold: None,
        agent_name: Some("trivial".to_string()),
        session_id: Some(session_id.clone()),
        working_dir: None,
        git_branch: None,
        git_remote: None,
        terminal_session_id: None,
        agent_variables: Default::default(),
        agent_instructions: "Complete the turn.".to_string(),
        model_fallbacks: vec![],
        compaction_agent: None,
    })
    .await
    .expect("seed local session header");

    let session = ThinClientSession::new(
        ThinClientConfig {
            cluster: LOCAL_CLUSTER_KEY.to_string(),
            agent: "trivial".to_string(),
            session_id: Some(session_id),
        },
        client,
        jetstream,
        harnx_runtime::utils::create_abort_signal(),
    )
    .await
    .expect("create local thin client");
    let result = tokio::time::timeout(
        Duration::from_secs(15),
        session.run_turn("hello", Arc::new(NullSink), None),
    )
    .await
    .expect("local worker turn timeout")
    .expect("local worker turn");
    assert_eq!(result.response.as_deref(), Some("worker completed"));
    mock_task.await.expect("mock OpenAI task");
}

#[cfg(unix)]
fn kill_process(pid: u32) {
    // SAFETY: PID belongs to worker child created by this test.
    let result = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    assert_eq!(
        result,
        0,
        "kill worker {pid}: {}",
        std::io::Error::last_os_error()
    );
}

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    // SAFETY: signal 0 only checks process existence/permission.
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(unix)]
async fn wait_for_process_exit(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while process_exists(pid) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        !process_exists(pid),
        "worker {pid} remained after supervisor drop"
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_worker_supervisor_deduplicates_respawns_and_tears_down() {
    require_nextest();
    let Some(binary) = skip_without_binaries() else {
        return;
    };
    let root = tempfile::tempdir().expect("create isolated supervisor environment");
    let _environment = isolated_environment(root.path());
    let (api_base, mock_task) = start_mock_openai().await;
    write_trivial_agent_config(root.path(), &api_base);

    let mut owner = LocalWorkerSupervisor::start_with_worker_binary(&binary)
        .await
        .expect("start local worker owner");
    assert!(owner.is_worker_owner());
    assert_eq!(
        worker_ready_subject(LOCAL_CLUSTER_KEY),
        "cluster.__local__.worker.ready"
    );
    assert_eq!(
        local_worker_lock_file().file_name(),
        Some(OsStr::new("worker.lock"))
    );
    let first_pid = owner.worker_pid().expect("owned worker PID");

    let joiner = LocalWorkerSupervisor::start_with_worker_binary(&binary)
        .await
        .expect("join existing local worker");
    assert!(
        !joiner.is_worker_owner(),
        "second front-end spawned a worker"
    );
    assert_eq!(joiner.worker_pid(), None);

    assert_worker_completes_turn(&owner, mock_task).await;

    kill_process(first_pid);
    let deadline = Instant::now() + Duration::from_secs(5);
    let replacement_pid = loop {
        owner.ensure().await.expect("respawn killed local worker");
        let pid = owner.worker_pid().expect("replacement worker PID");
        if pid != first_pid {
            break pid;
        }
        assert!(Instant::now() < deadline, "worker was not respawned");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(process_exists(replacement_pid));

    drop(owner);
    wait_for_process_exit(replacement_pid).await;
    drop(joiner);
}
