use super::*;

pub(super) struct TestNats {
    pub(super) url: String,
    child: Child,
    _store: tempfile::TempDir,
}

impl Drop for TestNats {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub(super) async fn spawn_nats() -> Option<TestNats> {
    let binary = which::which("nats-server").ok()?;
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .ok()?
        .local_addr()
        .ok()?
        .port();
    let store = tempfile::tempdir().ok()?;
    let mut child = Command::new(binary)
        .args(["-js", "-sd"])
        .arg(store.path())
        .args(["-p", &port.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let url = format!("nats://127.0.0.1:{port}");
    for _ in 0..50 {
        if async_nats::connect(&url).await.is_ok() {
            return Some(TestNats {
                url,
                child,
                _store: store,
            });
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}
