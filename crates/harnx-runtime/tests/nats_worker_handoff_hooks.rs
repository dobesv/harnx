//! Regression coverage for handoff losing hook enforcement.
//!
//! `run_agent_loop_with_nats_inner` resolves `hook_start_config` once, from
//! the activation agent's hooks. Before the fix, a handoff carried that same
//! `Option` unchanged, so a handoff to an agent WITH hooks that the
//! activation agent lacked never started them — `reconcile_hook_supervisor`
//! just no-ops on a `None` start. Kept in its own file rather than appended
//! to the already-large `nats_worker.rs`.

mod common;

use anyhow::Result;
use harnx_core::{
    hooks::{HookEvent, HookResultControl},
    instance::ServerScope,
    require_nextest,
    tool::ToolCall,
};
use harnx_runtime::{
    client::CompletionTokenUsage,
    config::Config,
    nats_hook_provider::{HookDispatchMeta, NatsHookProvider},
    nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease},
    nats_session_log::NatsSessionLog,
    nats_session_metadata::{SessionMetadata, SessionMetadataStore},
    nats_worker::{run_agent_loop_with_nats, NatsSessionLogBackend, RunAgentLoopArgs},
    utils::create_abort_signal,
};
use parking_lot::RwLock;
use serde_json::json;
use std::path::Path;
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

async fn seed_session_and_attach_runtime(
    global_config: &Arc<RwLock<Config>>,
    jetstream: async_nats::jetstream::Context,
    session_id: &str,
) -> Result<NatsSessionLog> {
    let metadata_store = SessionMetadataStore::ensure(&jetstream, 1).await?;
    let metadata = SessionMetadata::new(
        session_id,
        harnx_runtime::SessionInitializer::named("alpha", Default::default()),
    );
    metadata_store.create(&metadata).await?;
    let backend = NatsSessionLogBackend::new(jetstream.clone(), session_id)
        .with_metadata_store(Some(metadata_store));

    let log = NatsSessionLog::new(jetstream, session_id);
    let mut session = metadata.base_session();
    let runtime = std::sync::Arc::new(backend.clone())
        as std::sync::Arc<dyn harnx_runtime::config::session::SessionAppendSink>;
    session.runtime = Some(std::sync::Arc::new(runtime));
    global_config.write().session = Some(session);
    Ok(log)
}

/// First turn requests a handoff to `beta` (naming convention:
/// `<agent>_session_handoff`); the delegated turn then finishes with plain
/// text and no further tool calls.
fn make_beta_handoff_call_fn() -> harnx_runtime::agent_loop::AgentCallFn {
    let call_count = Arc::new(AtomicUsize::new(0));
    Arc::new(move |_input, _config, _abort| {
        let cc = call_count.clone();
        Box::pin(async move {
            let n = cc.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
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
    })
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

/// Load config, activate `alpha` for `session_id`, and seed the session log.
async fn build_activated_config(
    env: &TestEnv,
    session_id: &str,
) -> Result<(Arc<RwLock<Config>>, async_nats::jetstream::Context)> {
    let config_path = env.config_dir.join("config.yaml");
    let base = {
        let _config_guard = EnvVarGuard::set_path("HARNX_CONFIG_DIR", &env.config_dir);
        Config::load_from_file(&config_path)?
    };
    let config = Arc::new(RwLock::new(base));
    harnx_runtime::config::Config::use_agent(
        &config,
        "alpha",
        Some(session_id),
        create_abort_signal(),
    )
    .await?;

    let js = {
        let cfg = config.read().clone();
        cfg.nats_jetstream("local").await?
    };
    seed_session_and_attach_runtime(&config, js.clone(), session_id).await?;
    Ok((config, js))
}

/// Fresh post-run discovery: query NATS directly rather than through the
/// worker's own (now-dropped) `ctx.nats_hook_provider`, so this reflects
/// whatever registration the handoff's hook supervisor actually left behind.
async fn assert_pre_tool_use_blocked(
    js: &async_nats::jetstream::Context,
    instance_id: ServerScope,
) -> Result<()> {
    let provider = NatsHookProvider::discover_with_client(js.client().clone(), instance_id).await?;
    let outcome = provider
        .dispatch_event(
            HookEvent::PreToolUse {
                tool_name: "exec".to_string(),
                tool_input: json!({"command": "true"}),
                tool_use_id: "post-handoff-check".to_string(),
            },
            None,
            HookDispatchMeta {
                session_id: "handoff-hooks-remote-session".to_string(),
                cwd: std::env::current_dir()?,
                resume_count: 0,
            },
        )
        .await;
    assert!(
        matches!(outcome.control, HookResultControl::Block { .. }),
        "handoff to an agent with hooks must attempt to enforce them (fail-closed) even \
         when the hook server itself fails to start; got {:?}",
        outcome.control
    );
    Ok(())
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
    let (config, js) = build_activated_config(&env, session_id).await?;
    let metadata_store = SessionMetadataStore::ensure(&js, 1).await?;

    let lease = Arc::new(
        NatsSessionLease::acquire(NatsLeaseAcquireParams {
            jetstream: js.clone(),
            session_id,
            worker_id: "worker-handoff-hooks".to_string(),
            generation: 1,
            config: NatsLeaseConfig::default(),
            session_metadata: Some(metadata_store.clone()),
        })
        .await?
        .expect("acquire"),
    );

    let instance_id = ServerScope::new();
    let input = harnx_runtime::config::input::from_str(&config, "start handoff", None);
    run_agent_loop_with_nats(RunAgentLoopArgs {
        cluster_key: "local",
        manage_servers: true,
        session_id,
        config: config.clone(),
        instance_id: instance_id.clone(),
        initial_input: input,
        abort_signal: create_abort_signal(),
        call_fn: Some(make_beta_handoff_call_fn()),
        lease: Some(lease),
        lease_config: NatsLeaseConfig::default(),
        after_seq_observer: None,
        session_metadata: Some(&metadata_store),
        on_tool_round: None,
        working_dir: None,
    })
    .await?;

    assert_pre_tool_use_blocked(&js, instance_id).await
}
