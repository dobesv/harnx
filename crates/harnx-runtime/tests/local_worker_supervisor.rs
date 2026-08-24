// The supervisor uses Unix process groups/signals.
#![cfg(unix)]

#[allow(dead_code)]
mod common;

use harnx_core::{event::NullSink, require_nextest, session::SessionLogEntry};
use harnx_runtime::config::LOCAL_CLUSTER_KEY;
use harnx_runtime::local_orchestrator::LocalWorkerSupervisor;
use harnx_runtime::nats_session_log::NatsSessionLog;
use harnx_runtime::nats_worker::{
    targeted_consumer_name, targeted_worker_ready_subject, LocalWorkerTarget,
};
use harnx_runtime::utils::create_abort_signal;
use harnx_runtime::{NatsSession, NatsSessionConfig};
use std::ffi::OsString;
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
    let Some(binary) = common::harnx_worker_binary() else {
        eprintln!("skipping local worker supervisor test: harnx-worker binary not found");
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
    non_target: &LocalWorkerSupervisor,
    mock_task: tokio::task::JoinHandle<()>,
) {
    let client = async_nats::ConnectOptions::new()
        .token(supervisor.server().token.clone())
        .connect(&supervisor.server().url)
        .await
        .expect("connect to NATS");
    let jetstream = async_nats::jetstream::new(client.clone());
    let session_id = format!("local-supervisor-{}", uuid::Uuid::new_v4());
    let log = NatsSessionLog::new(jetstream.clone(), session_id.clone());
    log.append_event_async(&SessionLogEntry::Header {
        model_id: "mock:test".to_string(),
        temperature: None,
        top_p: None,
        use_tools: Some(vec![]),
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

    let session = NatsSession::new(
        NatsSessionConfig {
            cluster: LOCAL_CLUSTER_KEY.to_string(),
            agent: "trivial".to_string(),
            session_id: Some(session_id),
            activation_route: supervisor.route().activation_route(),
        },
        client,
        jetstream.clone(),
        create_abort_signal(),
    )
    .await
    .expect("create local NATS session");
    let result = tokio::time::timeout(
        Duration::from_secs(15),
        session.run_turn("hello", Arc::new(NullSink), None),
    )
    .await
    .expect("local worker turn timeout")
    .expect("local worker turn");
    assert_eq!(result.response.as_deref(), Some("worker completed"));
    let stream = jetstream
        .get_stream("LOCAL_WORK_NOTIFY_V2")
        .await
        .expect("get local-v2 activation stream");
    let non_target_consumer: async_nats::jetstream::consumer::PullConsumer = stream
        .get_consumer(&targeted_consumer_name(non_target.route().worker_id()).unwrap())
        .await
        .unwrap_or_else(|error| panic!("get non-target consumer: {error}"));
    assert_eq!(
        non_target_consumer
            .get_info()
            .await
            .expect("read non-target consumer info")
            .delivered
            .consumer_sequence,
        0,
        "frontend B's worker consumed frontend A's targeted turn"
    );
    mock_task.await.expect("mock OpenAI task");
}

fn kill_process(pid: u32) {
    let result = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    assert_eq!(
        result,
        0,
        "kill worker {pid}: {}",
        std::io::Error::last_os_error()
    );
}

fn process_exists(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

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

fn assert_distinct_frontend_workers(
    frontend_a: &LocalWorkerSupervisor,
    frontend_b: &LocalWorkerSupervisor,
) {
    assert_ne!(frontend_a.worker_pid(), frontend_b.worker_pid());
    assert_ne!(frontend_a.route(), frontend_b.route());
    assert_eq!(frontend_a.server().url, frontend_b.server().url);
    assert_eq!(
        targeted_worker_ready_subject(
            LocalWorkerTarget::new(
                frontend_a.route().session_scope(),
                frontend_a.route().worker_id()
            )
            .unwrap()
        ),
        format!(
            "session_scope.__local__.workers.{}.worker.ready",
            frontend_a.route().worker_id()
        )
    );
    assert_ne!(
        targeted_consumer_name(frontend_a.route().worker_id()).unwrap(),
        targeted_consumer_name(frontend_b.route().worker_id()).unwrap()
    );
}

async fn replace_inputs_and_start_fresh_frontend(
    root: &Path,
    binary: &Path,
    frontend_a: &mut LocalWorkerSupervisor,
    first_a_pid: u32,
) -> LocalWorkerSupervisor {
    std::fs::write(
        root.join("config/config.yaml"),
        "save: false\nstream: false\nclient: mock\nmodel: mock:test\n# changed\n",
    )
    .expect("update config");
    let rebuilt = binary.with_extension("rebuilt");
    std::fs::copy(binary, &rebuilt).expect("copy rebuilt fixture");
    std::fs::rename(rebuilt, binary).expect("replace worker fixture");
    let route_before = frontend_a.route().clone();
    let route_after = frontend_a
        .ensure(create_abort_signal())
        .await
        .expect("keep live worker");
    assert_eq!(route_before, route_after);
    assert_eq!(frontend_a.worker_pid(), Some(first_a_pid));

    let frontend_c = LocalWorkerSupervisor::start_with_worker_binary(binary, create_abort_signal())
        .await
        .expect("fresh frontend starts from replacement inputs");
    assert_ne!(frontend_c.worker_pid(), Some(first_a_pid));
    assert_ne!(frontend_c.route(), frontend_a.route());
    frontend_c
}

async fn crash_and_respawn(frontend: &mut LocalWorkerSupervisor, old_pid: u32) -> u32 {
    kill_process(old_pid);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        frontend
            .ensure(create_abort_signal())
            .await
            .expect("respawn frontend worker");
        let pid = frontend.worker_pid().expect("replacement worker PID");
        if pid != old_pid {
            return pid;
        }
        assert!(Instant::now() < deadline, "worker was not respawned");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn joined_supervisor_recovers_after_broker_owner_exits() {
    require_nextest();
    let Some(installed_binary) = skip_without_binaries() else {
        return;
    };
    let root = tempfile::tempdir().expect("create isolated supervisor environment");
    let binary = root.path().join("harnx-worker");
    std::fs::copy(installed_binary, &binary).expect("copy worker binary fixture");
    let _environment = isolated_environment(root.path());
    write_trivial_agent_config(root.path(), "http://127.0.0.1:1/v1");

    let owner = LocalWorkerSupervisor::start_with_worker_binary(&binary, create_abort_signal())
        .await
        .expect("start broker-owning frontend worker");
    let owner_worker_pid = owner.worker_pid().expect("owner worker PID");
    assert!(owner.server().is_owner());

    let mut joiner =
        LocalWorkerSupervisor::start_with_worker_binary(&binary, create_abort_signal())
            .await
            .expect("start broker-joining frontend worker");
    let joined_worker_pid = joiner.worker_pid().expect("joined worker PID");
    let joined_route = joiner.route().clone();
    let old_broker_nonce = joiner.server().nonce.clone();
    assert!(!joiner.server().is_owner());

    drop(owner);
    wait_for_process_exit(owner_worker_pid).await;
    assert!(
        process_exists(joined_worker_pid),
        "joined worker exited before broker recovery"
    );

    let recovered_route = joiner
        .ensure(create_abort_signal())
        .await
        .expect("recover joined worker after broker owner exits");
    let replacement_worker_pid = joiner.worker_pid().expect("replacement worker PID");
    assert_ne!(replacement_worker_pid, joined_worker_pid);
    wait_for_process_exit(joined_worker_pid).await;
    assert_eq!(recovered_route, joined_route);
    assert_eq!(joiner.route(), &joined_route);
    assert_ne!(joiner.server().nonce, old_broker_nonce);
    assert!(
        joiner.server().is_owner(),
        "surviving frontend must take ownership of the replacement broker"
    );
    async_nats::ConnectOptions::new()
        .token(joiner.server().token.clone())
        .connect(&joiner.server().url)
        .await
        .expect("connect to replacement broker")
        .flush()
        .await
        .expect("flush replacement broker connection");

    drop(joiner);
    wait_for_process_exit(replacement_worker_pid).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_supervisors_own_distinct_workers_routes_and_process_trees() {
    require_nextest();
    let Some(installed_binary) = skip_without_binaries() else {
        return;
    };
    let root = tempfile::tempdir().expect("create isolated supervisor environment");
    let binary = root.path().join("harnx-worker");
    std::fs::copy(installed_binary, &binary).expect("copy worker binary fixture");
    let _environment = isolated_environment(root.path());
    let (api_base, mock_task) = start_mock_openai().await;
    write_trivial_agent_config(root.path(), &api_base);

    let mut frontend_a =
        LocalWorkerSupervisor::start_with_worker_binary(&binary, create_abort_signal())
            .await
            .expect("start frontend A worker");
    let frontend_b =
        LocalWorkerSupervisor::start_with_worker_binary(&binary, create_abort_signal())
            .await
            .expect("start frontend B worker");
    let first_a_pid = frontend_a.worker_pid().expect("frontend A worker PID");
    let b_pid = frontend_b.worker_pid().expect("frontend B worker PID");
    assert_distinct_frontend_workers(&frontend_a, &frontend_b);

    assert_worker_completes_turn(&frontend_a, &frontend_b, mock_task).await;
    let route_before = frontend_a.route().clone();
    let frontend_c =
        replace_inputs_and_start_fresh_frontend(root.path(), &binary, &mut frontend_a, first_a_pid)
            .await;
    let c_pid = frontend_c.worker_pid().expect("frontend C worker PID");
    let replacement_a_pid = crash_and_respawn(&mut frontend_a, first_a_pid).await;
    assert_eq!(frontend_a.route(), &route_before);

    // Dropping A reaps only A's process group; B remains alive.
    drop(frontend_a);
    wait_for_process_exit(replacement_a_pid).await;
    assert!(
        process_exists(b_pid),
        "dropping frontend A terminated frontend B's worker"
    );
    assert!(
        process_exists(c_pid),
        "dropping frontend A terminated frontend C's worker"
    );
    drop(frontend_b);
    wait_for_process_exit(b_pid).await;
    drop(frontend_c);
    wait_for_process_exit(c_pid).await;
}
