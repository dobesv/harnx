//! Process and bus capture for the black-box OAuth leakage check.
#![cfg(test)]

use super::*;
use tokio::io::AsyncRead;
use tokio::task::JoinHandle;

pub(super) struct NativeProcess {
    child: tokio::process::Child,
    stdout: JoinHandle<Vec<u8>>,
    stderr: JoinHandle<Vec<u8>>,
}

impl NativeProcess {
    pub(super) async fn start(shared: &SharedNats, upstream: &str, scope: &ServerScope) -> Self {
        let mut child = tokio::process::Command::new(
            crate::common::proxy_binary_path().expect("build native binary first"),
        )
        .args(["--url", upstream, "--credentials-key-file"])
        .arg(&shared.config.credentials_key_file)
        .args([
            "--name",
            "jira",
            "--auth-methods",
            "oauth",
            "--oauth-scope",
            "offline_access",
            "--http-bind",
            "127.0.0.1:0",
        ])
        .env("HARNX_NATS_URL", shared.nats.url())
        .env_remove("HARNX_NATS_TOKEN")
        .env("HARNX_NATS_REPLICAS", "1")
        .env("HARNX_SERVER_SCOPE", scope.as_str())
        .env("HARNX_SERVER_CONFIG", "")
        .env("HARNX_SERVER_PACKAGE", "")
        .env("RUST_LOG", "trace")
        .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("native process spawn");
        let stdout = capture_pipe(child.stdout.take().expect("stdout"));
        let stderr = capture_pipe(child.stderr.take().expect("stderr"));
        Self {
            child,
            stdout,
            stderr,
        }
    }

    pub(super) async fn wait_registered(
        &mut self,
        js: &async_nats::jetstream::Context,
        scope: &ServerScope,
        identity: &str,
    ) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            if let Ok(registry) = js
                .get_key_value(harnx_toolset_server::TOOL_REGISTRY_BUCKET)
                .await
            {
                if registry
                    .get(format!("{}.{}", scope.as_str(), identity))
                    .await
                    .expect("registry read")
                    .is_some()
                {
                    break;
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "native registration deadline"
            );
            assert!(
                self.child.try_wait().expect("process status").is_none(),
                "native process exited before registration"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    pub(super) async fn stop_and_scan(mut self, secrets: &[String]) {
        self.child.start_kill().expect("stop native process");
        self.child.wait().await.expect("native process exit");
        let stdout = self.stdout.await.expect("stdout task");
        let stderr = self.stderr.await.expect("stderr task");
        assert!(!stderr.is_empty(), "process log capture wasn't exercised");
        assert_secret_free(&stdout, secrets, "native stdout");
        assert_secret_free(&stderr, secrets, "native trace logs");
    }
}

fn capture_pipe<R: AsyncRead + Unpin + Send + 'static>(mut pipe: R) -> JoinHandle<Vec<u8>> {
    tokio::spawn(async move {
        let mut bytes = Vec::new();
        pipe.read_to_end(&mut bytes)
            .await
            .expect("read native pipe");
        bytes
    })
}

pub(super) struct BusCapture {
    stop: CancellationToken,
    join: JoinHandle<Vec<Vec<u8>>>,
}

impl BusCapture {
    pub(super) async fn start(client: &async_nats::Client) -> Self {
        let mut subscriber = client.subscribe(">").await.expect("bus observer");
        client.flush().await.expect("observer ready");
        let stop = CancellationToken::new();
        let capture_stop = stop.clone();
        let join = tokio::spawn(async move {
            let mut messages = Vec::new();
            loop {
                tokio::select! {
                    _ = capture_stop.cancelled() => break,
                    message = subscriber.next() => match message {
                        Some(message) => {
                            messages.push(message.payload.to_vec());
                            messages.push(format!("{} {:?}", message.subject, message.headers).into_bytes());
                        }
                        None => break,
                    }
                }
            }
            messages
        });
        Self { stop, join }
    }

    pub(super) async fn stop_and_scan(self, client: &async_nats::Client, secrets: &[String]) {
        client.flush().await.expect("flush capture");
        tokio::time::sleep(Duration::from_millis(50)).await;
        self.stop.cancel();
        let messages = self.join.await.expect("bus capture task");
        assert!(!messages.is_empty());
        for bytes in &messages {
            assert_secret_free(bytes, secrets, "NATS payload");
        }
    }
}
