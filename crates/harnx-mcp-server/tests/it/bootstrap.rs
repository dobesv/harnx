#[path = "handler.rs"]
mod handler;
#[path = "real_mcp.rs"]
mod real_mcp;
#[path = "support.rs"]
mod support;

pub use support::*;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use harnx_mcp_server::bootstrap::{resolve_target_cluster, Bootstrap};
use harnx_runtime::{
    config::{Config, NatsRouting, LOCAL_CLUSTER_KEY},
    SessionActivationRoute,
};
pub use serde_json::json;
use std::{path::Path, sync::Arc};
use tokio_util::task::AbortOnDropHandle;

#[test]
fn cluster_resolution_precedence_and_local_spelling() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut config = Config::load_from_file(&fixture.config_dir().join("config.yaml"))?;
    config.apply_frontend_nats_routing();
    assert_eq!(resolve_target_cluster(&config, None)?, LOCAL_CLUSTER_KEY);
    assert_eq!(
        resolve_target_cluster(&config, Some(LOCAL_CLUSTER_KEY))?,
        LOCAL_CLUSTER_KEY
    );
    config.nats_routing = NatsRouting::Cluster("Y".to_owned());
    assert_eq!(resolve_target_cluster(&config, None)?, "Y");
    assert_eq!(resolve_target_cluster(&config, Some("X"))?, "X");
    assert_eq!(
        resolve_target_cluster(&config, Some(LOCAL_CLUSTER_KEY))?,
        LOCAL_CLUSTER_KEY
    );
    assert_eq!(
        resolve_target_cluster(&config, Some(" "))
            .unwrap_err()
            .to_string(),
        "--cluster value must not be empty"
    );
    Ok(())
}

#[tokio::test]
async fn unknown_cluster_validation_error() -> Result<()> {
    let fixture = Fixture::new()?;
    let error = fixture
        .bootstrap(Some("nonexistent"))
        .await
        .err()
        .context("unknown cluster accepted")?;
    assert_eq!(
        error.to_string(),
        "unknown NATS cluster 'nonexistent' (expected nats_servers/nonexistent.yaml)"
    );
    Ok(())
}

#[test]
fn binary_validates_cluster_before_transport_unavailable() -> Result<()> {
    let fixture = Fixture::new()?;
    for transport in ["--mcp-stdio", "--mcp-http"] {
        let output = std::process::Command::new(binary("harnx-mcp-server")?)
            .args([
                transport,
                "--use-tools",
                "fs_*",
                "--cluster",
                "nonexistent",
                "--config-dir",
            ])
            .arg(fixture.config_dir())
            .env("HARNX_NATS_SERVER", "Y")
            .output()?;
        assert!(!output.status.success());
        assert!(
            output.stdout.is_empty(),
            "startup must not write protocol stdout"
        );
        let stderr = String::from_utf8(output.stderr)?;
        assert!(
            stderr.contains(
                "unknown NATS cluster 'nonexistent' (expected nats_servers/nonexistent.yaml)"
            ),
            "{stderr}"
        );
        assert!(
            !stderr.contains("scaffold"),
            "validation must precede C4 error: {stderr}"
        );
    }
    Ok(())
}

#[test]
fn cli_tool_selectors_replace_environment_value() -> Result<()> {
    let fixture = Fixture::new()?;
    let output = std::process::Command::new(binary("harnx-mcp-server")?)
        .args([
            "--mcp-stdio",
            "--use-tools",
            "cli_only_*",
            "--cluster",
            "nonexistent",
        ])
        .arg("--config-dir")
        .arg(fixture.config_dir())
        .env("HARNX_MCP_USE_TOOLS", "env_only_*")
        // Assert selector values, not tracing's terminal styling or inherited filters.
        .env("NO_COLOR", "1")
        .env("RUST_LOG", "info")
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("selectors=[\"cli_only_*\"]"), "{stderr}");
    assert!(!stderr.contains("env_only_*"), "{stderr}");
    Ok(())
}

#[tokio::test]
async fn local_routing_is_lazy_and_cli_local_overrides_env() -> Result<()> {
    let fixture = Fixture::new()?;
    let _env = EnvGuard::set("HARNX_NATS_SERVER", Some("Y".as_ref()));
    let bootstrap = fixture.bootstrap(Some(LOCAL_CLUSTER_KEY)).await?;
    assert_eq!(bootstrap.cluster(), LOCAL_CLUSTER_KEY);
    assert_eq!(bootstrap.config().nats_routing, NatsRouting::FrontendLocal);
    assert!(bootstrap.config().session.is_none());
    let _worker = EnvGuard::set("HARNX_WORKER_BIN", Some("/nonexistent/worker".as_ref()));
    let again = fixture.bootstrap(Some(LOCAL_CLUSTER_KEY)).await?;
    assert_eq!(again.cluster(), LOCAL_CLUSTER_KEY);
    Ok(())
}

#[tokio::test]
async fn env_cluster_and_cli_override_get_shared_routes() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.cluster("X", "nats://127.0.0.1:1")?;
    fixture.cluster("Y", "nats://127.0.0.1:2")?;
    let _env = EnvGuard::set("HARNX_NATS_SERVER", Some("Y".as_ref()));
    for (arg, expected) in [(None, "Y"), (Some("X"), "X")] {
        let bootstrap = Arc::new(fixture.bootstrap(arg).await?);
        assert_eq!(bootstrap.cluster(), expected);
        assert_eq!(
            bootstrap.config().nats_routing,
            NatsRouting::Cluster(expected.to_owned())
        );
        let (one, two) = tokio::join!(bootstrap.ensure_route(), bootstrap.ensure_route());
        assert_eq!(one?, SessionActivationRoute::ClusterShared);
        assert_eq!(two?, SessionActivationRoute::ClusterShared);
    }
    Ok(())
}

struct LocalBootstrapFiles {
    config_dir: std::path::PathBuf,
    marker_path: std::path::PathBuf,
}

impl LocalBootstrapFiles {
    fn write(fixture: &Fixture) -> Result<Self> {
        let config_dir = fixture.root.path().join("explicit-config");
        std::fs::create_dir_all(config_dir.join("tool_servers"))?;
        std::fs::write(
            config_dir.join("config.yaml"),
            "save: false\nstream: false\n",
        )?;
        let marker_path = fixture.root.path().join("marker.txt");
        std::fs::write(&marker_path, "local bootstrap marker")?;
        let mut command = tokio::process::Command::new(binary("harnx-fs-tools")?);
        command.args([
            "--name",
            "fs",
            "--allow-read",
            fixture.root.path().to_str().unwrap(),
        ]);
        std::fs::write(
            config_dir.join("tool_servers/fs.yaml"),
            serde_json::to_vec(&serde_json::json!({
                "name": "fs",
                "command": binary("harnx-fs-tools")?,
                "args": ["--name", "fs", "--allow-read", fixture.root.path()],
            }))?,
        )?;
        Ok(Self {
            config_dir,
            marker_path,
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nats_local_concurrent_routes_and_reservations_share_worker() -> Result<()> {
    let fixture = Fixture::new()?;
    let files = LocalBootstrapFiles::write(&fixture)?;
    let _worker = EnvGuard::set(
        "HARNX_WORKER_BIN",
        Some(binary("harnx-worker")?.as_os_str()),
    );
    let bootstrap = Arc::new(
        Bootstrap::new(
            Some(LOCAL_CLUSTER_KEY.to_owned()),
            Some(files.config_dir),
            harnx_core::abort::create_abort_signal(),
        )
        .await?,
    );
    let (one, two) = tokio::time::timeout(DEADLINE, async {
        tokio::join!(bootstrap.ensure_route(), bootstrap.ensure_route())
    })
    .await?;
    let route = one?;
    assert_eq!(route, two?);
    assert!(
        matches!(route, SessionActivationRoute::WorkerTargeted { ref session_scope, .. } if session_scope == LOCAL_CLUSTER_KEY)
    );
    let (one, two) = tokio::join!(
        bootstrap.open_reservation(view("fs_*")),
        bootstrap.open_reservation(view("fs_*"))
    );
    let mut one = one?;
    let mut two = two?;
    assert_ne!(one.session_id(), two.session_id());
    assert_eq!(one.server_scope(), two.server_scope());
    assert!(bootstrap.config().session.is_none());
    assert_read(
        &discover(&one, "fs_read").await?,
        ReadToolAssertion {
            tool: "fs_read",
            path: &files.marker_path,
            marker: "local bootstrap marker",
        },
    )
    .await?;
    one.close().await?;
    assert_read(
        &discover(&two, "fs_read").await?,
        ReadToolAssertion {
            tool: "fs_read",
            path: &files.marker_path,
            marker: "local bootstrap marker",
        },
    )
    .await?;
    two.close().await?;
    Ok(())
}

struct DualClusterSetup {
    workers: [AbortOnDropHandle<Result<()>>; 2],
    fs_x: tokio::process::Child,
    fs_y: tokio::process::Child,
    _process_manager: harnx_core::child_process::ChildProcessManager,
    x_bootstrap: Arc<Bootstrap>,
    y_bootstrap: Arc<Bootstrap>,
    x_client: async_nats::Client,
    y_client: async_nats::Client,
    x_reserves: async_nats::Subscriber,
    y_reserves: async_nats::Subscriber,
    _env: [EnvGuard; 4],
    // Failed assertions still drop children before their brokers.
    _x: harnx_test_bins::NatsServerHandle,
    _y: harnx_test_bins::NatsServerHandle,
}

impl DualClusterSetup {
    async fn teardown(mut self) -> Result<()> {
        self.fs_x.kill().await?;
        self.fs_y.kill().await?;
        for worker in self.workers {
            worker.abort();
            assert!(worker.await.unwrap_err().is_cancelled());
        }
        Ok(())
    }
}

fn dual_cluster_env(secondary_url: &str) -> [EnvGuard; 4] {
    [
        EnvGuard::set("HARNX_NATS_SERVER", Some("Y".as_ref())),
        EnvGuard::set("HARNX_NATS_URL", Some(secondary_url.as_ref())),
        EnvGuard::set("HARNX_NATS_TOKEN", Some("".as_ref())),
        EnvGuard::set(
            "HARNX_SERVER_SCOPE",
            Some("same-scope-on-both-brokers".as_ref()),
        ),
    ]
}

async fn setup_dual_cluster(fixture: &Fixture) -> Result<DualClusterSetup> {
    let x = harnx_test_bins::spawn_nats_server()
        .await?
        .context("nats-server required for routing test")?;
    let y = harnx_test_bins::spawn_nats_server()
        .await?
        .context("nats-server required for routing test")?;
    assert_ne!(x.url(), y.url());
    fixture.cluster("X", x.url())?;
    fixture.cluster("Y", y.url())?;
    let env = dual_cluster_env(y.url());
    let x_bootstrap = Arc::new(fixture.bootstrap(Some("X")).await?);
    let y_bootstrap = Arc::new(fixture.bootstrap(None).await?);
    let workers = [
        start_worker(WorkerOptions {
            config: x_bootstrap.config().clone(),
            cluster: "X",
        })
        .await?,
        start_worker(WorkerOptions {
            config: y_bootstrap.config().clone(),
            cluster: "Y",
        })
        .await?,
    ];
    let process_manager = harnx_core::child_process::ChildProcessManager::new();
    let fs_x = start_fs(FsServerOptions {
        manager: &process_manager,
        url: x.url(),
        name: "fs",
        root: fixture.root.path(),
    })
    .await?;
    let fs_y = start_fs(FsServerOptions {
        manager: &process_manager,
        url: y.url(),
        name: "otherfs",
        root: fixture.root.path(),
    })
    .await?;
    let x_client = async_nats::connect(x.url()).await?;
    let y_client = async_nats::connect(y.url()).await?;
    let x_reserves = x_client.subscribe(reserve_subject("X")).await?;
    let y_reserves = y_client.subscribe(reserve_subject("Y")).await?;
    x_client.flush().await?;
    y_client.flush().await?;
    Ok(DualClusterSetup {
        _x: x,
        _y: y,
        _process_manager: process_manager,
        _env: env,
        x_bootstrap,
        y_bootstrap,
        x_client,
        y_client,
        x_reserves,
        y_reserves,
        fs_x,
        fs_y,
        workers,
    })
}

async fn verify_primary_reservation(
    bootstrap: &Bootstrap,
    client: &async_nats::Client,
    reserves: &mut async_nats::Subscriber,
    marker: &Path,
) -> Result<ToolReservationHandle> {
    let handle = bootstrap.open_reservation(view("fs_*")).await?;
    assert_eq!(
        handle.config().nats_routing,
        NatsRouting::Cluster("X".to_owned())
    );
    let x_metadata = harnx_runtime::nats_session_metadata::SessionMetadataStore::ensure(
        &async_nats::jetstream::new(client.clone()),
        1,
    )
    .await?;
    assert!(x_metadata
        .get(handle.session_storage_key())
        .await?
        .is_some());
    let x_request = tokio::time::timeout(DEADLINE, reserves.next())
        .await?
        .context("X reserve missing")?;
    let request: Reserve = serde_json::from_slice(&x_request.payload)?;
    assert_eq!(request.session_storage_key, handle.session_storage_key());
    assert_eq!(request.view, view("fs_*"));
    let provider = discover(&handle, "fs_read").await?;
    assert!(!provider
        .declarations()
        .iter()
        .any(|tool| tool.name == "otherfs_read"));
    assert_read(
        &provider,
        ReadToolAssertion {
            tool: "fs_read",
            path: marker,
            marker: "selected broker marker",
        },
    )
    .await?;
    assert!(bootstrap.config().session.is_none());
    Ok(handle)
}

async fn verify_secondary_reservation(
    bootstrap: &Bootstrap,
    reserves: &mut async_nats::Subscriber,
    marker: &Path,
) -> Result<ToolReservationHandle> {
    let y_handle = bootstrap.open_reservation(view("otherfs_*")).await?;
    let y_request = tokio::time::timeout(DEADLINE, reserves.next())
        .await?
        .context("Y reserve missing")?;
    let request: Reserve = serde_json::from_slice(&y_request.payload)?;
    assert_eq!(request.session_storage_key, y_handle.session_storage_key());
    assert_read(
        &discover(&y_handle, "otherfs_read").await?,
        ReadToolAssertion {
            tool: "otherfs_read",
            path: marker,
            marker: "selected broker marker",
        },
    )
    .await?;
    Ok(y_handle)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nats_cli_x_over_env_y_reserves_discovers_and_calls_x() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut setup = setup_dual_cluster(&fixture).await?;
    let marker = fixture.root.path().join("marker.txt");
    std::fs::write(&marker, "selected broker marker")?;

    let y_metadata = harnx_runtime::nats_session_metadata::SessionMetadataStore::ensure(
        &async_nats::jetstream::new(setup.y_client.clone()),
        1,
    )
    .await?;

    let mut handle = verify_primary_reservation(
        &setup.x_bootstrap,
        &setup.x_client,
        &mut setup.x_reserves,
        &marker,
    )
    .await?;
    assert!(y_metadata
        .get(handle.session_storage_key())
        .await?
        .is_none());

    let mut y_handle =
        verify_secondary_reservation(&setup.y_bootstrap, &mut setup.y_reserves, &marker).await?;

    assert_eq!(handle.server_scope(), y_handle.server_scope());
    handle.close().await?;
    y_handle.close().await?;
    setup.teardown().await?;
    Ok(())
}
