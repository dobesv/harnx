//! Regression coverage for handoff losing hook enforcement.
//!
//! `run_agent_loop_with_nats_inner` resolves `hook_start_config` once, from
//! the activation agent's hooks. Before the fix, a handoff carried that same
//! `Option` unchanged, so a handoff to an agent WITH hooks that the
//! activation agent lacked never started them — `reconcile_hook_supervisor`
//! just no-ops on a `None` start. Kept in its own file rather than appended
//! to the already-large `nats_worker.rs`.

mod common;

use anyhow::{Context, Result};
use futures_util::TryStreamExt;
use harnx_core::{event::NullSink, require_nextest, session::SessionLogEntry, tool::ToolCall};
use harnx_hookset::{HOOK_EXPECTATIONS_BUCKET, HOOK_REGISTRY_BUCKET};
use harnx_runtime::{
    client::CompletionTokenUsage,
    config::Config,
    nats_session::{NatsSession, NatsSessionConfig},
    nats_session_log::NatsSessionLog,
    nats_session_metadata::SessionInitializer,
    nats_worker::{run_worker_daemon, WorkerDaemonConfig},
    utils::create_abort_signal,
};
use parking_lot::RwLock;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

async fn require_nats_server() -> Result<Option<common::NatsServerHandle>> {
    require_nextest();
    let Some(server) = common::spawn_nats_server().await? else {
        eprintln!("skipping: nats-server not available");
        return Ok(None);
    };
    Ok(Some(server))
}

/// Build the hook server used by lifecycle tests when Cargo did not already
/// place it next to the test executables.
async fn ensure_hook_server_binary() -> Result<()> {
    let mut path = std::env::current_exe()?;
    path.pop();
    if path.file_name().is_some_and(|name| name == "deps") {
        path.pop();
    }
    path.push(if cfg!(windows) {
        "harnx-claude-compatible-hook-server.exe"
    } else {
        "harnx-claude-compatible-hook-server"
    });
    if path_is_file(&path).await? {
        return Ok(());
    }

    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| anyhow::anyhow!("resolve workspace root"))?
        .to_path_buf();
    let status =
        tokio::process::Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
            .args(["build", "-p", "harnx-claude-compatible-hook-server"])
            .current_dir(workspace)
            .status()
            .await?;
    anyhow::ensure!(status.success(), "building hook server failed");
    anyhow::ensure!(
        path_is_file(&path).await?,
        "hook server not found at {}",
        path.display()
    );
    Ok(())
}

/// Check a test artifact path without blocking a Tokio worker thread.
async fn path_is_file(path: &Path) -> Result<bool> {
    match tokio::fs::metadata(path).await {
        Ok(metadata) => Ok(metadata.is_file()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

struct EnvVarGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn set_path(key: &'static str, value: &Path) -> Self {
        let previous = std::env::var_os(key);
        unsafe { std::env::set_var(key, value) };
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => unsafe { std::env::set_var(self.key, value) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

/// Write the fixed `alpha`/`beta` agent pair this test needs: an activation
/// agent with no hooks handing off to one that declares a hook.
fn write_test_agents(config_dir: &Path) -> Result<()> {
    let agents_dir = config_dir.join("agents");
    std::fs::create_dir_all(&agents_dir)?;
    std::fs::write(
        agents_dir.join("alpha.md"),
        "---\nmodel: openai:test-model\nuse_tools: beta_session_handoff\n---\nAlpha agent instructions\n",
    )?;
    std::fs::write(
        agents_dir.join("beta.md"),
        "---\nmodel: openai:test-model\nhooks:\n  entries:\n    - command: \"true\"\n      status_message: \"beta agent safety hook\"\n---\nBeta agent instructions\n",
    )?;
    Ok(())
}

/// Write the healthy gated agent and hook-free handoff target used to verify
/// route cleanup across activations.
async fn write_cleanup_test_agents(config_dir: &Path) -> Result<()> {
    let agents_dir = config_dir.join("agents");
    tokio::fs::write(
        agents_dir.join("gated.md"),
        "---\nmodel: openai:test-model\nuse_tools:\n- target_session_handoff\nhooks:\n  entries:\n    - command: >-\n        harnx-claude-compatible-hook-server\n        --event PreToolUse\n        --matcher '^target_session_handoff$'\n        --jaq '{}'\n---\nGated agent instructions\n",
    )
    .await?;
    tokio::fs::write(
        agents_dir.join("target.md"),
        "---\nmodel: openai:test-model\n---\nTarget agent instructions\n",
    )
    .await?;
    Ok(())
}

/// First turn requests a handoff to `beta` (naming convention:
/// `<agent>_session_handoff`); the delegated turn then finishes with plain
/// text and no further tool calls.
fn make_beta_handoff_call_fn() -> harnx_runtime::agent_loop::AgentCallFn {
    let call_fn: harnx_runtime::agent_loop::AgentCallFn =
        Arc::new(move |_input, config, _abort| {
            let agent = config.read().extract_agent().name().to_string();
            Box::pin(async move {
                if agent == "alpha" {
                    Ok((
                        "handoff requested".to_string(),
                        None,
                        vec![ToolCall::new(
                            "beta_session_handoff".to_string(),
                            json!({
                                "prompt": "finish delegated work",
                                "session_id": "handoff-hooks-remote-session"
                            }),
                            Some("handoff-hooks-call-1".to_string()),
                            None,
                        )],
                        CompletionTokenUsage::default(),
                    ))
                } else {
                    Ok((
                        "handoff completed".to_string(),
                        None,
                        vec![],
                        CompletionTokenUsage::default(),
                    ))
                }
            })
        });
    call_fn
}

/// Complete one turn, request a handoff on the next gated-agent turn, and
/// complete every target-agent turn normally.
fn make_repeated_activation_call_fn() -> harnx_runtime::agent_loop::AgentCallFn {
    let gated_calls = Arc::new(AtomicUsize::new(0));
    Arc::new(move |_input, config, _abort| {
        let agent = config.read().extract_agent().name().to_string();
        let call_index = if agent == "gated" {
            gated_calls.fetch_add(1, Ordering::SeqCst)
        } else {
            0
        };
        Box::pin(async move {
            let tool_calls = if agent == "gated" && call_index == 1 {
                vec![ToolCall::new(
                    "target_session_handoff".to_string(),
                    json!({
                        "prompt": "finish after the gated handoff",
                        "session_id": "hook-cleanup-target"
                    }),
                    Some("hook-cleanup-handoff".to_string()),
                    None,
                )]
            } else {
                Vec::new()
            };
            Ok((
                "activation completed".to_string(),
                None,
                tool_calls,
                CompletionTokenUsage::default(),
            ))
        })
    })
}

/// Hold the model call open until the worker receives a cooperative user
/// cancellation, allowing the test to observe hook cleanup on the error path.
fn make_cancellable_call_fn(
    model_started: Arc<tokio::sync::Notify>,
) -> harnx_runtime::agent_loop::AgentCallFn {
    Arc::new(move |_input, _config, abort| {
        let model_started = Arc::clone(&model_started);
        Box::pin(async move {
            model_started.notify_one();
            harnx_runtime::utils::wait_abort_signal(&abort).await;
            anyhow::bail!("model call cancelled")
        })
    })
}

/// Return every live route key, propagating bucket and key-list failures so a
/// cleanup assertion cannot accidentally treat an unreadable bucket as empty.
async fn hook_bucket_keys(client: &async_nats::Client, bucket: &str) -> Result<Vec<String>> {
    let store = async_nats::jetstream::new(client.clone())
        .get_key_value(bucket)
        .await?;
    let keys = store.keys().await?;
    Ok(keys.try_collect().await?)
}

/// Require both hook discovery buckets to contain no activation-scoped routes.
async fn assert_hook_routes_cleaned(client: &async_nats::Client) -> Result<()> {
    for bucket in [HOOK_REGISTRY_BUCKET, HOOK_EXPECTATIONS_BUCKET] {
        let keys = hook_bucket_keys(client, bucket).await?;
        anyhow::ensure!(keys.is_empty(), "stale hook routes in {bucket}: {keys:?}");
    }
    Ok(())
}

/// Wait for cooperative cancellation to finish the worker-side shutdown path.
async fn wait_for_hook_routes_cleaned(client: &async_nats::Client) -> Result<()> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let registry = hook_bucket_keys(client, HOOK_REGISTRY_BUCKET).await?;
        let expectations = hook_bucket_keys(client, HOOK_EXPECTATIONS_BUCKET).await?;
        if registry.is_empty() && expectations.is_empty() {
            return Ok(());
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "stale hook routes after cancellation: registry={registry:?}, expectations={expectations:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// Wait for the handoff target's expected assistant response and durable turn
/// boundary, failing immediately if the target records an error.
async fn wait_for_successful_target_turn(target_log: &NatsSessionLog) -> Result<()> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let entries = target_log.load_events_async().await?;
        let has_expected_response = entries.iter().any(|(_, entry)| {
            matches!(
                entry,
                SessionLogEntry::Message { role, content, .. }
                    if role.is_assistant() && content.to_text() == "activation completed"
            )
        });
        let has_turn_end = entries
            .iter()
            .any(|(_, entry)| matches!(entry, SessionLogEntry::TurnEnd { .. }));
        if has_expected_response && has_turn_end {
            return Ok(());
        }
        anyhow::ensure!(
            !entries
                .iter()
                .any(|(_, entry)| matches!(entry, SessionLogEntry::Error { .. })),
            "handoff target failed after hook cleanup: {entries:?}"
        );
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "handoff target was not activated after hook cleanup: {entries:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// Directory layout plus env guards for one test run: config/data/state
/// dirs, the `alpha`/`beta` agent files, and the env vars that point agent
/// hook servers and the `local` cluster at `server`.
struct TestEnv {
    config_dir: std::path::PathBuf,
    _root: tempfile::TempDir,
    _config_guard: EnvVarGuard,
    _data_guard: EnvVarGuard,
    _state_guard: EnvVarGuard,
    _nats_url_guard: EnvVarGuard,
    _nats_token_guard: EnvVarGuard,
}

fn setup_test_env(server_url: &str) -> Result<TestEnv> {
    let root = tempfile::tempdir()?;
    let config_dir = root.path().join("config");
    let data_dir = root.path().join("data");
    let state_dir = root.path().join("state");
    std::fs::create_dir_all(&config_dir)?;
    std::fs::create_dir_all(config_dir.join("clients"))?;
    std::fs::create_dir_all(config_dir.join("nats_servers"))?;
    std::fs::create_dir_all(&data_dir)?;
    std::fs::create_dir_all(&state_dir)?;

    write_test_agents(&config_dir)?;

    let config_guard = EnvVarGuard::set_path("HARNX_CONFIG_DIR", &config_dir);
    let data_guard = EnvVarGuard::set_path("HARNX_DATA_DIR", &data_dir);
    let state_guard = EnvVarGuard::set_path("HARNX_STATE_DIR", &state_dir);
    // Complete env handoff so agent-level hook servers resolve against this
    // test's own broker instead of spinning up a separate shared local one.
    let nats_url_guard = EnvVarGuard::set_path("HARNX_NATS_URL", Path::new(server_url));
    let nats_token_guard = EnvVarGuard::set_path("HARNX_NATS_TOKEN", Path::new("test-token"));

    std::fs::write(config_dir.join("config.yaml"), "model: openai:test-model\n")?;
    std::fs::write(
        config_dir.join("nats_servers").join("local.yaml"),
        format!("url: {server_url}\n"),
    )?;
    std::fs::write(
        config_dir.join("clients").join("openai.yaml"),
        "type: openai\napi_key: sk-test\nmodels:\n  - name: test-model\n    type: chat\n    max_input_tokens: 4096\n",
    )?;

    Ok(TestEnv {
        config_dir,
        _root: root,
        _config_guard: config_guard,
        _data_guard: data_guard,
        _state_guard: state_guard,
        _nats_url_guard: nats_url_guard,
        _nats_token_guard: nats_token_guard,
    })
}

/// Load the worker config and connect to the test cluster.
async fn build_test_config(
    env: &TestEnv,
) -> Result<(
    Arc<RwLock<Config>>,
    async_nats::Client,
    async_nats::jetstream::Context,
)> {
    let config_path = env.config_dir.join("config.yaml");
    let base = {
        let _config_guard = EnvVarGuard::set_path("HARNX_CONFIG_DIR", &env.config_dir);
        Config::load_from_file(&config_path)?
    };
    let config = Arc::new(RwLock::new(base));
    let (client, js) = {
        let cfg = config.read().clone();
        (
            cfg.nats_client("local").await?,
            cfg.nats_jetstream("local").await?,
        )
    };
    Ok((config, client, js))
}

/// Regression test for the handoff hook-enforcement gap: activation agent
/// `alpha` has no hooks (so activation resolves `hook_start_config` to
/// `None`); handoff target `beta` declares a hook. Before the fix, handoff
/// reused alpha's `None` start config unchanged, so beta's hook supervisor
/// never even attempted to start and nothing was ever registered in NATS for
/// it. Prove this by discovering fresh from NATS after the run completes: a
/// hook-server start attempt (even one that itself fails, like the "true"
/// binary below, which doesn't speak the registration handshake) installs a
/// fail-closed rejector that blocks PreToolUse. No attempt at all leaves
/// PreToolUse open.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handoff_to_agent_with_hooks_starts_its_hook_enforcement() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let session_id = "handoff-hooks-root";
    let env = setup_test_env(server.url())?;
    let (config, client, js) = build_test_config(&env).await?;
    let call_fn = make_beta_handoff_call_fn();
    let daemon_config = WorkerDaemonConfig::managing("local", "worker-handoff-hooks");
    let daemon = tokio::spawn({
        let config = config.clone();
        async move { run_worker_daemon(config, daemon_config, Some(call_fn)).await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let source = NatsSession::new(
        NatsSessionConfig {
            cluster: "local".to_string(),
            initializer: SessionInitializer::named("alpha", Default::default()),
            session_id: Some(session_id.to_string()),
            activation_route: harnx_runtime::SessionActivationRoute::ClusterShared,
        },
        client,
        js.clone(),
        create_abort_signal(),
    )
    .await?;
    source
        .run_turn("start handoff", Arc::new(NullSink), None)
        .await?;

    let target_log = NatsSessionLog::new(js, "handoff-hooks-remote-session");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    let target_entries = loop {
        let entries = target_log.load_events_async().await?;
        if entries
            .iter()
            .any(|(_, entry)| matches!(entry, SessionLogEntry::TurnEnd { .. }))
        {
            break entries;
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "target hook-enforced turn did not finish"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    assert!(
        !target_entries.iter().any(|(_, entry)| matches!(
            entry,
            SessionLogEntry::Message { role, .. } if role.is_assistant()
        )),
        "beta's failed hook must block the target model response: {target_entries:?}"
    );

    daemon.abort();
    let _ = daemon.await;
    Ok(())
}

/// A completed activation must stop its healthy agent hook through the
/// supervisor's explicit cleanup path. Merely dropping the supervisor kills
/// the child and lets its monitor retain a fail-closed expectation; the next
/// activation then discovers that dead route and blocks the handoff with a
/// NATS `no responders` error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_agent_turn_cleans_hook_routes_before_next_handoff() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    ensure_hook_server_binary().await?;
    let env = setup_test_env(server.url())?;
    write_cleanup_test_agents(&env.config_dir).await?;
    let (config, client, js) = build_test_config(&env).await?;
    let daemon_config = WorkerDaemonConfig::managing("local", "worker-hook-cleanup");
    let daemon = tokio::spawn({
        let config = config.clone();
        async move {
            run_worker_daemon(
                config,
                daemon_config,
                Some(make_repeated_activation_call_fn()),
            )
            .await
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let source = NatsSession::new(
        NatsSessionConfig {
            cluster: "local".to_string(),
            initializer: SessionInitializer::named("gated", Default::default()),
            session_id: Some("hook-cleanup-source".to_string()),
            activation_route: harnx_runtime::SessionActivationRoute::ClusterShared,
        },
        client.clone(),
        js.clone(),
        create_abort_signal(),
    )
    .await?;
    source
        .run_turn("complete without tools", Arc::new(NullSink), None)
        .await?;
    assert_hook_routes_cleaned(&client).await?;

    source
        .run_turn("handoff now", Arc::new(NullSink), None)
        .await?;
    assert_hook_routes_cleaned(&client).await?;

    let target_log = NatsSessionLog::new(js, "hook-cleanup-target");
    wait_for_successful_target_turn(&target_log).await?;

    daemon.abort();
    let _ = daemon.await;
    Ok(())
}

/// User cancellation is cooperative: the model call returns through the
/// agent-loop error path, which must still await explicit hook shutdown.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_agent_turn_cleans_hook_routes() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    ensure_hook_server_binary().await?;
    let env = setup_test_env(server.url())?;
    write_cleanup_test_agents(&env.config_dir).await?;
    let (config, client, js) = build_test_config(&env).await?;
    let model_started = Arc::new(tokio::sync::Notify::new());
    let daemon_config = WorkerDaemonConfig::managing("local", "worker-hook-cancellation");
    let daemon = tokio::spawn({
        let config = config.clone();
        let call_fn = make_cancellable_call_fn(Arc::clone(&model_started));
        async move { run_worker_daemon(config, daemon_config, Some(call_fn)).await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let source = NatsSession::new(
        NatsSessionConfig {
            cluster: "local".to_string(),
            initializer: SessionInitializer::named("gated", Default::default()),
            session_id: Some("hook-cancellation-source".to_string()),
            activation_route: harnx_runtime::SessionActivationRoute::ClusterShared,
        },
        client.clone(),
        js,
        create_abort_signal(),
    )
    .await?;
    let (cancel_tx, cancel_rx) = tokio::sync::mpsc::channel(1);
    let turn = tokio::spawn(async move {
        source
            .run_turn("wait for cancellation", Arc::new(NullSink), Some(cancel_rx))
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(30), model_started.notified())
        .await
        .context("model call did not start")?;
    cancel_tx.send(()).await?;
    let turn_result = tokio::time::timeout(std::time::Duration::from_secs(30), turn)
        .await
        .context("cancelled turn did not return")??;
    let turn_result = turn_result?;
    anyhow::ensure!(
        turn_result.was_cancelled,
        "turn did not report cancellation"
    );
    wait_for_hook_routes_cleaned(&client).await?;

    daemon.abort();
    let _ = daemon.await;
    Ok(())
}
