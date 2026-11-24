//! Exercise production listeners while NATS bootstrap and HTTP drain are held open.
use super::*;
use std::net::{SocketAddr, TcpListener};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::watch,
};

struct NatsGate {
    address: SocketAddr,
    release: watch::Sender<bool>,
    _task: AbortOnDropHandle<()>,
}

impl NatsGate {
    async fn start(upstream: String) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let (release, wait) = watch::channel(false);
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            while let Ok((mut downstream, _)) = listener.accept().await {
                let upstream = upstream.clone();
                let mut wait = wait.clone();
                connections.spawn(async move {
                    wait.wait_for(|released| *released).await?;
                    let mut upstream = TcpStream::connect(upstream).await?;
                    tokio::io::copy_bidirectional(&mut downstream, &mut upstream).await?;
                    Ok::<_, anyhow::Error>(())
                });
            }
        });
        Ok(Self {
            address,
            release,
            _task: AbortOnDropHandle::new(task),
        })
    }
}

async fn wait_status(client: &reqwest::Client, url: &str, expected: u16) -> Result<()> {
    tokio::time::timeout(DEADLINE, async {
        loop {
            if let Ok(response) = client.get(url).send().await {
                if response.status().as_u16() == expected {
                    assert!(response.bytes().await?.is_empty());
                    return Ok::<_, anyhow::Error>(());
                }
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .with_context(|| format!("waiting for {url} to return {expected}"))?
}

struct ObservabilityServer {
    // Reap the frontend before stopping its NATS gate, worker or broker.
    process: Process,
    client: reqwest::Client,
    app: SocketAddr,
    healthz_url: String,
    metrics_url: String,
    gate: NatsGate,
    _harness: Harness,
}

impl ObservabilityServer {
    async fn start() -> Result<Self> {
        let h = Harness::start(Script::Text).await?;
        let upstream = h.config.read().nats_server("runner")?.url.clone();
        let gate = NatsGate::start(upstream.trim_start_matches("nats://").to_owned()).await?;
        std::fs::write(
            h.config_dir().join("nats_servers/runner.yaml"),
            format!("url: nats://{}\n", gate.address),
        )?;
        let (process, [app, healthz, metrics]) = Self::spawn(&h)?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()?;
        Ok(Self {
            process,
            client,
            app,
            healthz_url: format!("http://{healthz}/healthz"),
            metrics_url: format!("http://{metrics}/metrics"),
            gate,
            _harness: h,
        })
    }

    fn spawn(h: &Harness) -> Result<(Process, [SocketAddr; 3])> {
        // Reserve distinct ephemeral ports together before handing them to the child.
        let listeners = (0..3)
            .map(|_| TcpListener::bind("127.0.0.1:0"))
            .collect::<std::io::Result<Vec<_>>>()?;
        let addresses = listeners
            .iter()
            .map(TcpListener::local_addr)
            .collect::<std::io::Result<Vec<_>>>()?;
        let [app, healthz, metrics] = addresses.as_slice() else {
            unreachable!()
        };
        let log = std::fs::File::create(h.config_dir().join("observability.log"))?;
        let mut command = Command::new(binary("harnx-a2a-server")?);
        command
            .arg("--config-dir")
            .arg(h.config_dir())
            .args(["--cluster", "runner", "--agent", "runner", "--port"])
            .arg(app.port().to_string())
            .arg("--healthz-addr")
            .arg(healthz.to_string())
            .arg("--metrics-addr")
            .arg(metrics.to_string())
            .env_remove("HARNX_A2A_AGENTS")
            .env_remove("HARNX_HEALTHZ_ADDR")
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        drop(listeners);
        let process = Process::spawn("A2A observability server", command)?;
        Ok((process, [*app, *healthz, *metrics]))
    }

    async fn assert_readiness_transition(&self) -> Result<()> {
        wait_status(&self.client, &self.healthz_url, 503).await?;
        assert!(TcpStream::connect(self.app).await.is_err());
        self.gate.release.send(true)?;
        wait_status(&self.client, &self.healthz_url, 200).await
    }

    async fn assert_http_metrics(&self) -> Result<()> {
        let response = self
            .client
            .get(format!(
                "http://{}/agents/runner/.well-known/agent-card.json",
                self.app
            ))
            .send()
            .await?;
        assert_eq!(response.status(), 200);
        let response = self.client.get(&self.metrics_url).send().await?;
        assert_eq!(response.status(), 200);
        let metrics = response.text().await?;
        let counter = metrics
            .lines()
            .find(|line| line.starts_with("harnx_http_requests_total{"))
            .context("HTTP request counter missing from Prometheus scrape")?;
        assert!(counter.contains("method=\"GET\""), "{counter}");
        assert!(counter.contains("status=\"200\""), "{counter}");
        assert!(
            counter.contains("route=\"/agents/{name}/{*path}\""),
            "{counter}"
        );
        assert!(counter.ends_with(" 1"), "{counter}");
        assert!(metrics.contains("harnx_http_request_duration_seconds_count{"));
        assert!(!metrics.contains("/agents/runner"));
        Ok(())
    }

    async fn assert_shutdown_readiness(&mut self) -> Result<()> {
        // Expect: 100-continue proves the handler is reading this unfinished body.
        // Keep it open so shutdown's 503 is observable before the process exits.
        let mut request = TcpStream::connect(self.app).await?;
        request.write_all(b"POST /agents/runner HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nA2A-Version: 1.0\r\nContent-Length: 2\r\nExpect: 100-continue\r\nConnection: close\r\n\r\n").await?;
        let mut interim = [0; 25];
        tokio::time::timeout(DEADLINE, request.read_exact(&mut interim)).await??;
        assert_eq!(&interim, b"HTTP/1.1 100 Continue\r\n\r\n");
        anyhow::ensure!(
            Command::new("kill")
                .args(["-TERM", &self.process.child.id().to_string()])
                .status()?
                .success(),
            "send SIGTERM to A2A server"
        );
        wait_status(&self.client, &self.healthz_url, 503).await?;
        assert!(self.process.child.try_wait()?.is_none());
        request.write_all(b"{}").await?;
        let mut response = Vec::new();
        tokio::time::timeout(DEADLINE, request.read_to_end(&mut response)).await??;
        self.process.wait_success().await?;
        Ok(())
    }
}

#[tokio::test]
async fn healthz_bootstrap_shutdown_and_prometheus_http_metrics() -> Result<()> {
    let mut server = ObservabilityServer::start().await?;
    server.assert_readiness_transition().await?;
    server.assert_http_metrics().await?;
    server.assert_shutdown_readiness().await
}
