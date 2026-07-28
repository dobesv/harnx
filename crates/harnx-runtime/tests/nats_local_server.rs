use harnx_core::require_nextest;
use harnx_runtime::nats_local_server::ensure_shared_server;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

struct DataDirGuard {
    previous: Option<OsString>,
}

impl DataDirGuard {
    fn isolated(directory: &Path) -> Self {
        let previous = std::env::var_os("HARNX_DATA_DIR");
        // Nextest executes each test in its own process, so this process-wide
        // test override cannot race another test.
        unsafe { std::env::set_var("HARNX_DATA_DIR", directory) };
        Self { previous }
    }
}

impl Drop for DataDirGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => unsafe { std::env::set_var("HARNX_DATA_DIR", value) },
            None => unsafe { std::env::remove_var("HARNX_DATA_DIR") },
        }
    }
}

fn nats_server_binary() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("NATS_SERVER_BIN") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    which::which("nats-server").ok()
}

fn skip_without_nats_server() -> bool {
    if nats_server_binary().is_none() {
        eprintln!("skipping NATS integration test: nats-server binary not found");
        true
    } else {
        false
    }
}

fn isolated_data_dir() -> (TempDir, DataDirGuard) {
    let directory = tempfile::tempdir().expect("create isolated HARNX_DATA_DIR");
    let guard = DataDirGuard::isolated(directory.path());
    (directory, guard)
}

async fn authenticated_client(
    server: &harnx_runtime::nats_local_server::SharedNatsServer,
) -> async_nats::Client {
    async_nats::ConnectOptions::new()
        .token(server.token.clone())
        .connect(&server.url)
        .await
        .expect("connect to shared local NATS with minted token")
}

fn assert_server_configuration(server: &harnx_runtime::nats_local_server::SharedNatsServer) {
    let config_path = harnx_core::config_paths::nats_runtime_dir().join("nats.conf");
    let config = std::fs::read_to_string(&config_path).expect("read generated NATS config");
    assert!(config.contains("host: \"127.0.0.1\""));
    assert!(config.contains("authorization { token:"));
    assert!(config.contains(&server.token));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&config_path)
                .expect("stat generated NATS config")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    #[cfg(target_os = "linux")]
    {
        let pid = server.server_process_id().expect("owner has server PID");
        let cmdline =
            std::fs::read(format!("/proc/{pid}/cmdline")).expect("read nats-server command line");
        assert!(
            !cmdline
                .windows(server.token.len())
                .any(|window| window == server.token.as_bytes()),
            "broker token must not appear in process argv"
        );
        let args: Vec<&[u8]> = cmdline
            .split(|byte| *byte == 0)
            .filter(|arg| !arg.is_empty())
            .collect();
        assert_eq!(args.get(1).copied(), Some(b"-c".as_slice()));
        assert_eq!(
            args.get(2).copied(),
            Some(config_path.as_os_str().as_encoded_bytes())
        );
    }
}

#[tokio::test]
async fn nats_local_server_start_returns_usable_client() {
    require_nextest();
    if skip_without_nats_server() {
        return;
    }
    let (_directory, _guard) = isolated_data_dir();

    let server = ensure_shared_server()
        .await
        .expect("start shared local NATS");
    assert!(server.is_owner());

    assert_server_configuration(&server);

    let unauthenticated = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        async_nats::connect(&server.url),
    )
    .await;
    assert!(
        !matches!(unauthenticated, Ok(Ok(_))),
        "broker must reject connections without token"
    );

    let client = authenticated_client(&server).await;
    let mut subscriber = client
        .subscribe("harnx.test.local-server")
        .await
        .expect("subscribe");
    client
        .publish("harnx.test.local-server", "working".into())
        .await
        .expect("publish");
    client.flush().await.expect("flush publish");
    let message = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        futures_util::StreamExt::next(&mut subscriber).await
    })
    .await
    .expect("published message timed out")
    .expect("subscriber closed");
    assert_eq!(message.payload.as_ref(), b"working");
}

#[tokio::test]
async fn nats_local_server_concurrent_ensure_reuses_owner() {
    require_nextest();
    if skip_without_nats_server() {
        return;
    }
    let (_directory, _guard) = isolated_data_dir();

    let (first, second) = tokio::join!(ensure_shared_server(), ensure_shared_server());
    let first = first.expect("first ensure succeeds");
    let second = second.expect("second ensure succeeds");

    assert_ne!(
        first.is_owner(),
        second.is_owner(),
        "exactly one caller owns child"
    );
    assert_eq!(first.url, second.url);
    assert_eq!(first.token, second.token);
    assert_eq!(first.nonce, second.nonce);
    authenticated_client(&first)
        .await
        .flush()
        .await
        .expect("reused server remains usable");
}

#[tokio::test]
async fn nats_local_server_owner_drop_starts_fresh_owner() {
    require_nextest();
    if skip_without_nats_server() {
        return;
    }
    let (_directory, _guard) = isolated_data_dir();

    let first = ensure_shared_server().await.expect("start first owner");
    assert!(first.is_owner());
    let first_nonce = first.nonce.clone();
    let config_path = harnx_core::config_paths::nats_runtime_dir().join("nats.conf");
    assert!(config_path.exists());
    drop(first);
    assert!(!config_path.exists(), "owner drop removes token config");

    let second = ensure_shared_server()
        .await
        .expect("start replacement owner");
    assert!(second.is_owner());
    assert_ne!(
        first_nonce, second.nonce,
        "replacement must publish fresh nonce"
    );
    authenticated_client(&second)
        .await
        .flush()
        .await
        .expect("replacement server is usable");
}

#[tokio::test]
async fn local_cluster_jetstream_connects_to_shared_server_with_token() {
    require_nextest();
    if skip_without_nats_server() {
        return;
    }
    let (_directory, _guard) = isolated_data_dir();
    // Exercise manager fallback rather than env handoff. Nextest gives this
    // test its own process, so clearing these variables cannot race another test.
    unsafe {
        std::env::remove_var(harnx_runtime::config::HARNX_NATS_URL_ENV);
        std::env::remove_var(harnx_runtime::config::HARNX_NATS_TOKEN_ENV);
    }

    let server = ensure_shared_server()
        .await
        .expect("start shared local NATS");
    let config = harnx_runtime::config::Config::default();
    let jetstream = config
        .nats_jetstream(harnx_runtime::config::LOCAL_CLUSTER_KEY)
        .await
        .expect("connect reserved local cluster with shared token");

    let stream_name = format!("LOCAL_CLUSTER_{}", uuid::Uuid::new_v4().simple());
    jetstream
        .create_stream(async_nats::jetstream::stream::Config {
            name: stream_name,
            subjects: vec!["harnx.test.local-cluster".to_string()],
            ..Default::default()
        })
        .await
        .expect("create stream through reserved local JetStream context");

    assert!(server.is_owner());
}
