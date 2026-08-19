use super::*;

fn live_worker_binary() -> Option<std::path::PathBuf> {
    if std::process::Command::new(
        std::env::var_os("NATS_SERVER_BIN").unwrap_or_else(|| "nats-server".into()),
    )
    .arg("--version")
    .output()
    .is_err()
    {
        eprintln!("skipping serve NATS smoke: nats-server not available");
        return None;
    }
    if let Some(path) = std::env::var_os("HARNX_WORKER_BIN").map(std::path::PathBuf::from) {
        if path.is_file() {
            return Some(path);
        }
    }
    let mut directory = std::env::current_exe().ok()?.parent()?.to_path_buf();
    if directory.file_name().is_some_and(|name| name == "deps") {
        directory.pop();
    }
    let binary = directory.join(if cfg!(windows) {
        "harnx-worker.exe"
    } else {
        "harnx-worker"
    });
    binary.is_file().then_some(binary)
}

async fn read_http_request(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
    use tokio::io::AsyncReadExt;
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let read = socket.read(&mut buffer).await.expect("read mock request");
        if read == 0 {
            return request;
        }
        request.extend_from_slice(&buffer[..read]);
        if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&request[..end]);
            let length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            if request.len() >= end + 4 + length {
                return request;
            }
        }
    }
}

struct ServeSmokeOpenAi {
    api_base: String,
    first_request: tokio::sync::oneshot::Receiver<()>,
    release_first: tokio::sync::oneshot::Sender<()>,
    second_request: tokio::sync::oneshot::Receiver<Vec<u8>>,
    task: tokio::task::JoinHandle<()>,
}

async fn start_serve_smoke_openai() -> ServeSmokeOpenAi {
    use tokio::io::AsyncWriteExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock OpenAI");
    let address = listener.local_addr().expect("mock address");
    let (first_tx, first_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let (second_tx, second_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.expect("first model request");
        read_http_request(&mut first).await;
        let _ = first_tx.send(());
        let _ = release_rx.await;
        let body = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"serve worker completed\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(), body
        );
        first
            .write_all(response.as_bytes())
            .await
            .expect("first response");

        let (mut second, _) = listener.accept().await.expect("second model request");
        let request = read_http_request(&mut second).await;
        let _ = second_tx.send(request);
        tokio::time::sleep(Duration::from_secs(30)).await;
    });
    ServeSmokeOpenAi {
        api_base: format!("http://{address}/v1"),
        first_request: first_rx,
        release_first: release_tx,
        second_request: second_rx,
        task,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serve_replays_prompt_queued_during_active_run() {
    harnx_core::require_nextest();
    let Some(binary) = live_worker_binary() else {
        return;
    };
    let _guard = harnx_runtime::client::TestStateGuard::new(None).await;
    let sandbox = TestConfigSandbox::new();
    let mut mock = start_serve_smoke_openai().await;
    sandbox.write_mock_openai_client(&mock.api_base);
    sandbox.write_agent_with_front_matter(
        "plain",
        "model: mock:test\nstream: true",
        "Complete the turn.",
    );
    let config = sandbox.config();
    let supervisor = LocalWorkerSupervisor::start_with_worker_binary(binary, create_abort_signal())
        .await
        .expect("start local worker");
    let registry = SessionRegistry::new_with_local_worker_for_tests(config, supervisor);
    let session_id = format!("serve-nats-replay-{}", uuid::Uuid::new_v4());
    let handle = registry.get_or_spawn(key("plain", &session_id));
    let _events = subscribe(&handle).await.events;

    assert!(matches!(
        prompt(&handle, "first prompt").await,
        PromptResult::Accepted { .. }
    ));
    mock.first_request
        .await
        .expect("first mock request notifier dropped");
    assert!(matches!(
        prompt(&handle, "queued follow-up").await,
        PromptResult::Enqueued { .. }
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(250), &mut mock.second_request)
            .await
            .is_err(),
        "queued prompt started before active run completed"
    );

    let _ = mock.release_first.send(());
    let second_request = tokio::time::timeout(Duration::from_secs(15), mock.second_request)
        .await
        .expect("queued NATS session turn did not reach worker")
        .expect("second mock request notifier dropped");
    assert!(
        String::from_utf8_lossy(&second_request).contains("queued follow-up"),
        "second model request did not contain queued prompt"
    );
    cancel(&handle).await;
    mock.task.abort();
}

async fn assert_first_turn_streamed_and_finished(
    events: &mut tokio::sync::broadcast::Receiver<Event>,
) {
    let saw_chunk = tokio::time::timeout(Duration::from_secs(15), async {
        let mut saw_chunk = false;
        loop {
            match events.recv().await.expect("first turn event") {
                Event::TextMessageContent(_) => saw_chunk = true,
                Event::RunFinished(_) => return saw_chunk,
                Event::RunError(error) => panic!("first run failed: {}", error.message),
                _ => {}
            }
        }
    })
    .await
    .expect("first serve NATS turn timeout");
    assert!(saw_chunk, "NATS advisory did not reach AG-UI text stream");
}

async fn wait_for_cancelled_run(events: &mut tokio::sync::broadcast::Receiver<Event>) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if matches!(
                events.recv().await.expect("cancel turn event"),
                Event::RunFinished(_)
            ) {
                return;
            }
        }
    })
    .await
    .expect("cancelled serve NATS turn did not finish");
}

async fn assert_cancel_reached_worker(config: &harnx_runtime::config::Config, session_id: &str) {
    let jetstream = config
        .nats_jetstream(LOCAL_CLUSTER_KEY)
        .await
        .expect("local JetStream");
    let entries = harnx_runtime::nats_session_log::NatsSessionLog::new(jetstream, session_id)
        .load_events_async()
        .await
        .expect("load cancelled session");
    assert!(
        entries
            .iter()
            .any(|(_, entry)| matches!(entry, harnx_core::session::SessionLogEntry::Cancel { .. })),
        "web cancel did not reach worker control listener"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serve_local_nats_smoke_streams_sse_events_and_cancel_publishes_control() {
    harnx_core::require_nextest();
    let Some(binary) = live_worker_binary() else {
        return;
    };
    let _guard = harnx_runtime::client::TestStateGuard::new(None).await;
    let sandbox = TestConfigSandbox::new();
    let mock = start_serve_smoke_openai().await;
    sandbox.write_mock_openai_client(&mock.api_base);
    sandbox.write_agent_with_front_matter(
        "plain",
        "model: mock:test\nstream: true",
        "Complete the turn.",
    );
    let config = sandbox.config();
    let supervisor = LocalWorkerSupervisor::start_with_worker_binary(binary, create_abort_signal())
        .await
        .expect("start local worker");
    let registry = SessionRegistry::new_with_local_worker_for_tests(config.clone(), supervisor);
    let session_id = format!("serve-nats-smoke-{}", uuid::Uuid::new_v4());
    let handle = registry.get_or_spawn(key("plain", &session_id));
    let mut events = subscribe(&handle).await.events;

    assert!(matches!(
        prompt(&handle, "first prompt").await,
        PromptResult::Accepted { .. }
    ));
    mock.first_request
        .await
        .expect("first mock request notifier dropped");
    let _ = mock.release_first.send(());
    assert_first_turn_streamed_and_finished(&mut events).await;

    assert!(matches!(
        prompt(&handle, "cancel this prompt").await,
        PromptResult::Accepted { .. }
    ));
    tokio::time::timeout(Duration::from_secs(30), mock.second_request)
        .await
        .expect("worker did not start cancellable request")
        .expect("mock request notifier dropped");
    cancel(&handle).await;
    wait_for_cancelled_run(&mut events).await;
    assert_cancel_reached_worker(&config, &session_id).await;
    mock.task.abort();
}
