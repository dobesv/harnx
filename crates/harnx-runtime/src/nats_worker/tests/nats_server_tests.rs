use super::{env_lock, spawn_test_nats, TestEnvGuard};

#[tokio::test]
async fn spawn_test_nats_skips_only_when_binary_is_missing() {
    let _lock = env_lock().await;
    let _binary = TestEnvGuard::remove("NATS_SERVER_BIN");
    let _path = TestEnvGuard::new("PATH", "");
    assert!(spawn_test_nats().await.is_none());
}

#[tokio::test]
#[should_panic(expected = "failed to start NATS test server: spawn")]
async fn spawn_test_nats_panics_on_spawn_failure() {
    let _lock = env_lock().await;
    // An existing directory is discoverable but cannot be executed on any platform.
    let directory = tempfile::tempdir().unwrap();
    let _binary = TestEnvGuard::new("NATS_SERVER_BIN", directory.path());
    let _path = TestEnvGuard::new("PATH", "");
    let _ = spawn_test_nats().await;
}

#[tokio::test]
async fn spawn_test_nats_honors_binary_override_and_preserves_guards() {
    let _lock = env_lock().await;
    let Some(binary) = std::env::var_os("NATS_SERVER_BIN")
        .map(std::path::PathBuf::from)
        .filter(|path| path.exists())
        .or_else(|| which::which("nats-server").ok())
    else {
        return;
    };
    let _binary = TestEnvGuard::new("NATS_SERVER_BIN", binary);
    let _path = TestEnvGuard::new("PATH", "");
    let (url, mut server, store) = spawn_test_nats()
        .await
        .expect("binary override must be used");
    let client = async_nats::connect(&url).await.unwrap();
    client.flush().await.unwrap();
    assert!(store.path().is_dir());
    server.kill().unwrap();
    server.wait().unwrap();
    let store_path = store.path().to_owned();
    drop(server);
    assert!(store_path.is_dir(), "store guard must remain caller-owned");
    drop(store);
    assert!(!store_path.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn spawn_test_nats_retries_early_exit_then_panics() {
    use futures_util::FutureExt;
    use std::os::unix::fs::PermissionsExt;

    let _lock = env_lock().await;
    let directory = tempfile::tempdir().unwrap();
    let binary = directory.path().join("nats-server");
    let attempts = binary.with_extension("attempts");
    // Record each fresh process without relying on anything being on PATH.
    std::fs::write(
        &binary,
        "#!/bin/sh\nprintf 'attempt\\n' >> \"$0.attempts\"\nexit 42\n",
    )
    .unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let _binary = TestEnvGuard::new("NATS_SERVER_BIN", &binary);
    let _path = TestEnvGuard::new("PATH", "");
    let panic = std::panic::AssertUnwindSafe(spawn_test_nats())
        .catch_unwind()
        .await
        .err()
        .expect("early server exit must panic, not skip");
    let message = panic.downcast_ref::<String>().unwrap();
    assert!(
        message.contains("nats-server exited during startup"),
        "{message}"
    );
    assert_eq!(
        std::fs::read_to_string(attempts).unwrap().lines().count(),
        5
    );
}
