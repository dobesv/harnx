//! End-to-end coverage for agents whose prompts interpolate variables loaded
//! from files (the `variables: [{name, path}]` frontmatter used throughout the
//! pantheon package), and for what a client sees when the worker can't render
//! the prompt at all.
//!
//! The worker resolves the agent from its own config dir on activation. If it
//! skips the file-backed variables, the first template render fails with an
//! undefined value and — unless the failure is recorded durably — the client
//! waits for a reply that never arrives.

mod common;

use anyhow::{Context, Result};
use common::{spawn_nats_server, NatsServerHandle};
use harnx_core::{
    abort::create_abort_signal, event::NullSink, require_nextest, session::SessionLogEntry,
};
use harnx_runtime::{
    config::Config,
    nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease},
    nats_session_log::NatsSessionLog,
    nats_worker::{run_worker_daemon, WorkerDaemonConfig},
    ThinClientConfig, ThinClientSession, ThinClientTurnResult,
};
use parking_lot::RwLock;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

const FILE_VARIABLE_AGENT: &str = "varagent";
const BROKEN_AGENT: &str = "brokenagent";
const UNLOADABLE_AGENT: &str = "unloadableagent";
const PACKAGE_NAME: &str = "testpkg";
const PACKAGE_AGENT_STEM: &str = "assembled";
/// Shared prompt fragments the package agent stitches together, mirroring
/// `packages/pantheon/agents/shared/*.md`.
const PACKAGE_SHARED_FILES: [(&str, &str); 3] = [
    ("core.md", "CORE: you are the assembled agent."),
    ("review.md", "REVIEW: always run the quality gate."),
    ("delegation.md", "DELEGATION: hand off research tasks."),
];
const VARIABLE_FILE_TEXT: &str = "core instructions loaded from a file";
/// Generous enough that a slow CI box does not trip it, short enough that a
/// genuinely stalled turn fails the test rather than hanging the suite.
const TURN_TIMEOUT: Duration = Duration::from_secs(20);

struct EnvVarGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn set_path(key: &'static str, value: &Path) -> Self {
        let previous = std::env::var_os(key);
        // SAFETY: nextest gives each test its own process, so nothing else
        // mutates the environment concurrently.
        unsafe { std::env::set_var(key, value) };
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        // SAFETY: see EnvVarGuard::set_path.
        unsafe {
            match &self.previous {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

/// A temp config dir wired to a NATS server, plus the env guards that point
/// harnx at it. Guards must outlive every worker and client in the test.
struct TestEnv {
    server: NatsServerHandle,
    config_dir: std::path::PathBuf,
    _root: tempfile::TempDir,
    _guards: Vec<EnvVarGuard>,
}

impl TestEnv {
    fn new(server: NatsServerHandle) -> Result<Self> {
        let root = tempfile::tempdir()?;
        let config_dir = root.path().join("config");
        let data_dir = root.path().join("data");
        let state_dir = root.path().join("state");
        std::fs::create_dir_all(config_dir.join("clients"))?;
        std::fs::create_dir_all(config_dir.join("nats_servers"))?;
        std::fs::create_dir_all(&data_dir)?;
        std::fs::create_dir_all(&state_dir)?;

        std::fs::write(config_dir.join("config.yaml"), "model: openai:test-model\n")?;
        std::fs::write(
            config_dir.join("nats_servers").join("local.yaml"),
            format!("url: {}\n", server.url()),
        )?;
        std::fs::write(
            config_dir.join("clients").join("openai.yaml"),
            "type: openai\napi_key: sk-test\nmodels:\n  - name: test-model\n    type: chat\n    max_input_tokens: 4096\n",
        )?;

        let guards = vec![
            EnvVarGuard::set_path("HARNX_CONFIG_DIR", &config_dir),
            EnvVarGuard::set_path("HARNX_DATA_DIR", &data_dir),
            EnvVarGuard::set_path("HARNX_STATE_DIR", &state_dir),
        ];

        Ok(Self {
            server,
            config_dir,
            _root: root,
            _guards: guards,
        })
    }

    fn write_agent(&self, name: &str, body: &str) -> Result<()> {
        let agents_dir = self.config_dir.join("agents");
        std::fs::create_dir_all(&agents_dir)?;
        std::fs::write(agents_dir.join(format!("{name}.md")), body)?;
        Ok(())
    }

    /// The pantheon layout: a variable whose value is a sibling markdown file.
    fn write_file_backed_variable_agent(&self) -> Result<()> {
        let shared_dir = self.config_dir.join("agents").join("shared");
        std::fs::create_dir_all(&shared_dir)?;
        std::fs::write(shared_dir.join("core.md"), VARIABLE_FILE_TEXT)?;
        self.write_agent(
            FILE_VARIABLE_AGENT,
            "---\n\
             model: openai:test-model\n\
             variables:\n\
             - name: agent_core\n\
             \x20 description: Core instructions\n\
             \x20 path: shared/core.md\n\
             ---\n\
             Preamble.\n\n{{agent_core}}\n",
        )
    }

    /// An agent whose prompt references a variable that is never defined.
    fn write_undefined_variable_agent(&self) -> Result<()> {
        self.write_agent(
            BROKEN_AGENT,
            "---\nmodel: openai:test-model\n---\nPreamble.\n\n{{never_defined}}\n",
        )
    }

    /// An agent the worker can find but cannot load: its model names a client
    /// that is not configured.
    fn write_unloadable_agent(&self) -> Result<()> {
        self.write_agent(
            UNLOADABLE_AGENT,
            "---\nmodel: nosuchclient:nosuchmodel\n---\nPreamble.\n",
        )
    }

    /// A package agent assembled from several shared files, plus an inline
    /// default and a variable left for the caller to supply — the shape every
    /// pantheon agent has.
    ///
    /// Package agents resolve through `<config>/packages/<pkg>/agents/<stem>.md`
    /// rather than `<config>/agents/<name>.md`, so their `path:` variables
    /// resolve against a different directory than a plain agent's.
    fn write_package_agent(&self) -> Result<()> {
        let package_dir = self.config_dir.join("packages").join(PACKAGE_NAME);
        let agents_dir = package_dir.join("agents");
        std::fs::create_dir_all(agents_dir.join("shared"))?;
        std::fs::create_dir_all(package_dir.join("clients"))?;
        for (file, body) in PACKAGE_SHARED_FILES {
            std::fs::write(agents_dir.join("shared").join(file), body)?;
        }
        std::fs::write(
            package_dir.join("package.yaml"),
            format!("name: {PACKAGE_NAME}\nversion: 0.0.0\n"),
        )?;
        // Package agents resolve their model's client name relative to the
        // package, so the client must live in the package too — the same way
        // pantheon ships `clients/claude.yaml` for `model: claude:...`.
        std::fs::write(
            package_dir.join("clients").join("openai.yaml"),
            "type: openai\napi_key: sk-test\nmodels:\n  - name: test-model\n    type: chat\n    max_input_tokens: 4096\n",
        )?;
        std::fs::write(
            agents_dir.join(format!("{PACKAGE_AGENT_STEM}.md")),
            "---\n\
             model: openai:test-model\n\
             variables:\n\
             - name: core\n\
             \x20 description: Core identity\n\
             \x20 path: shared/core.md\n\
             - name: review\n\
             \x20 description: Review protocol\n\
             \x20 path: shared/review.md\n\
             - name: delegation\n\
             \x20 description: Delegation protocol\n\
             \x20 path: shared/delegation.md\n\
             - name: tone\n\
             \x20 description: Response tone\n\
             \x20 default: terse and direct\n\
             - name: repo\n\
             \x20 description: Repository under work\n\
             ---\n\
             {{core}}\n\n{{review}}\n\n{{delegation}}\n\nTone: {{tone}}\nRepo: {{repo}}\n",
        )?;
        Ok(())
    }

    async fn spawn_worker(&self, worker_id: &'static str) -> Result<tokio::task::JoinHandle<()>> {
        self.spawn_worker_with(worker_id, fixed_reply_call_fn("stub reply"), &[])
            .await
    }

    /// Spawn a worker with `--agent-variable`-style overrides, as
    /// `run_worker_command` sets them from the CLI.
    async fn spawn_worker_with(
        &self,
        worker_id: &'static str,
        call_fn: harnx_runtime::agent_loop::AgentCallFn,
        agent_variables: &[(&str, &str)],
    ) -> Result<tokio::task::JoinHandle<()>> {
        let mut config = Config::load_from_file(&self.config_dir.join("config.yaml"))?;
        // `run_worker_command` builds its config with `Config::init(.., true)`.
        config.info_flag = true;
        if !agent_variables.is_empty() {
            let mut vars = harnx_core::agent_config::AgentVariables::new();
            for (name, value) in agent_variables {
                vars.insert((*name).to_string(), (*value).to_string());
            }
            config.agent_variables = Some(vars);
        }
        let config = Arc::new(RwLock::new(config));
        let handle = tokio::spawn(async move {
            let _ = run_worker_daemon(
                config,
                WorkerDaemonConfig::new("local", worker_id),
                Some(call_fn),
            )
            .await;
        });
        tokio::time::sleep(Duration::from_millis(500)).await;
        Ok(handle)
    }

    /// Link a package straight out of the workspace so the worker loads the
    /// agents we actually ship, not a stand-in. Returns `None` when the package
    /// assets are absent (published crates do not carry them).
    fn link_workspace_package(&self, package: &str) -> Result<Option<()>> {
        let Some(workspace_root) = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .find(|ancestor| ancestor.join("packages").join(package).is_dir())
        else {
            return Ok(None);
        };
        let packages_dir = self.config_dir.join("packages");
        std::fs::create_dir_all(&packages_dir)?;
        let source = workspace_root.join("packages").join(package);
        let link = packages_dir.join(package);
        #[cfg(unix)]
        std::os::unix::fs::symlink(&source, &link)?;
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&source, &link)?;
        Ok(Some(()))
    }

    async fn jetstream(&self) -> Result<async_nats::jetstream::Context> {
        let client = async_nats::connect(self.server.url()).await?;
        Ok(async_nats::jetstream::new(client))
    }

    async fn run_turn(&self, agent: &str, session_id: &str) -> Result<ThinClientTurnResult> {
        let client = async_nats::connect(self.server.url()).await?;
        let jetstream = async_nats::jetstream::new(client.clone());
        let thin = ThinClientSession::new(
            ThinClientConfig {
                cluster: "local".to_string(),
                agent: agent.to_string(),
                session_id: Some(session_id.to_string()),
            },
            client,
            jetstream,
            create_abort_signal(),
        )
        .await?;

        tokio::time::timeout(
            TURN_TIMEOUT,
            thin.run_turn("hello", Arc::new(NullSink), None),
        )
        .await
        .context("turn stalled: the client never stopped waiting for the worker")?
    }
}

/// Records the system message the worker would send to the model, by running
/// the same `build_messages` the real call path uses. Asserting on this rather
/// than on the variable map proves the prompt the model sees is fully
/// interpolated.
fn prompt_capturing_call_fn(
    captured: Arc<std::sync::Mutex<Option<String>>>,
) -> harnx_runtime::agent_loop::AgentCallFn {
    Arc::new(move |input, config, _abort| {
        let system = harnx_runtime::config::input::build_messages(input, config)
            .ok()
            .and_then(|messages| {
                messages
                    .into_iter()
                    .find(|message| message.role == harnx_core::message::MessageRole::System)
                    .map(|message| message.content.to_text())
            });
        *captured.lock().unwrap() = system;
        Box::pin(async move {
            Ok((
                "stub reply".to_string(),
                None,
                vec![],
                harnx_runtime::client::CompletionTokenUsage::default(),
            ))
        })
    })
}

fn fixed_reply_call_fn(reply: &'static str) -> harnx_runtime::agent_loop::AgentCallFn {
    Arc::new(move |_input, _config, _abort| {
        Box::pin(async move {
            Ok((
                reply.to_string(),
                None,
                vec![],
                harnx_runtime::client::CompletionTokenUsage::default(),
            ))
        })
    })
}

async fn load_entries(
    jetstream: &async_nats::jetstream::Context,
    session_id: &str,
) -> Result<Vec<(u64, SessionLogEntry)>> {
    NatsSessionLog::new(jetstream.clone(), session_id)
        .load_events_async()
        .await
}

/// The header's recorded agent variables. The worker inserts the header for a
/// headerless session through an `EditEntries` replacement, so the mutations
/// have to be applied before the header is visible.
fn header_agent_variables(entries: &[(u64, SessionLogEntry)]) -> Result<Vec<(String, String)>> {
    harnx_core::session_reconstruct::apply_log_mutations_nats(entries)?
        .iter()
        .find_map(|(_, entry)| match entry {
            SessionLogEntry::Header {
                agent_variables, ..
            } => Some(
                agent_variables
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            ),
            _ => None,
        })
        .context("session header must record agent variables")
}

fn error_entry_messages(entries: &[(u64, SessionLogEntry)]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|(_, entry)| match entry {
            SessionLogEntry::Error { message, .. } => Some(message.clone()),
            _ => None,
        })
        .collect()
}

/// A worker activation for an agent with a file-backed variable must render its
/// prompt and answer the turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_renders_agent_prompt_with_file_backed_variables() -> Result<()> {
    require_nextest();

    let Some(server) = spawn_nats_server().await? else {
        eprintln!("skipping: nats-server not available");
        return Ok(());
    };

    let env = TestEnv::new(server)?;
    env.write_file_backed_variable_agent()?;
    let worker = env.spawn_worker("worker-agent-variables").await?;

    let session_id = "agent-file-backed-variables";
    let turn = env.run_turn(FILE_VARIABLE_AGENT, session_id).await?;

    assert_eq!(turn.error, None, "turn must not report a worker failure");
    assert_eq!(
        turn.response.as_deref(),
        Some("stub reply"),
        "worker must answer a turn for an agent with file-backed variables"
    );

    let entries = load_entries(&env.jetstream().await?, session_id).await?;
    let variables = header_agent_variables(&entries)?;
    assert!(
        variables
            .iter()
            .any(|(name, value)| name == "agent_core" && value == VARIABLE_FILE_TEXT),
        "header must carry the file-loaded variable value, got {variables:?}"
    );

    worker.abort();
    let _ = worker.await;
    Ok(())
}

/// A pantheon-shaped package agent: several `path:` fragments, an inline
/// `default:`, and one variable supplied by the worker's `--agent-variable`.
/// All of them must be interpolated into the prompt the model receives.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_assembles_package_agent_prompt_from_multiple_files() -> Result<()> {
    require_nextest();

    let Some(server) = spawn_nats_server().await? else {
        eprintln!("skipping: nats-server not available");
        return Ok(());
    };

    let env = TestEnv::new(server)?;
    env.write_package_agent()?;

    let captured = Arc::new(std::sync::Mutex::new(None));
    let worker = env
        .spawn_worker_with(
            "worker-package-agent",
            prompt_capturing_call_fn(Arc::clone(&captured)),
            &[("repo", "harnx")],
        )
        .await?;

    let agent = format!("{PACKAGE_NAME}/{PACKAGE_AGENT_STEM}");
    let turn = env.run_turn(&agent, "agent-package-assembled").await?;

    assert_eq!(turn.error, None, "turn must not report a worker failure");
    assert_eq!(turn.response.as_deref(), Some("stub reply"));

    let prompt = captured
        .lock()
        .unwrap()
        .clone()
        .context("worker must have built a system message for the turn")?;
    for (file, body) in PACKAGE_SHARED_FILES {
        assert!(
            prompt.contains(body),
            "prompt is missing the contents of shared/{file}; got:\n{prompt}"
        );
    }
    assert!(
        prompt.contains("Tone: terse and direct"),
        "inline default must be interpolated; got:\n{prompt}"
    );
    assert!(
        prompt.contains("Repo: harnx"),
        "worker --agent-variable must be interpolated; got:\n{prompt}"
    );
    assert!(
        !prompt.contains("{{"),
        "prompt must be fully interpolated; got:\n{prompt}"
    );

    worker.abort();
    let _ = worker.await;
    Ok(())
}

/// The real thing: activate a shipped pantheon agent, whose prompt is stitched
/// together from a dozen `shared/*.md` fragments. This is the case from #1365,
/// driven through the worker rather than a stand-in package.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_activates_shipped_pantheon_agent() -> Result<()> {
    require_nextest();

    let Some(server) = spawn_nats_server().await? else {
        eprintln!("skipping: nats-server not available");
        return Ok(());
    };

    let env = TestEnv::new(server)?;
    if env.link_workspace_package("pantheon")?.is_none() {
        eprintln!("skipping: packages/pantheon not available");
        return Ok(());
    }

    let captured = Arc::new(std::sync::Mutex::new(None));
    let worker = env
        .spawn_worker_with(
            "worker-pantheon",
            prompt_capturing_call_fn(Arc::clone(&captured)),
            &[],
        )
        .await?;

    let turn = env.run_turn("pantheon/sisyphus", "agent-pantheon").await?;
    assert_eq!(
        turn.error, None,
        "a shipped pantheon agent must activate cleanly"
    );

    let prompt = captured
        .lock()
        .unwrap()
        .clone()
        .context("worker must have built a system message for the turn")?;
    // The variable that broke in the original report.
    let core = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .find(|a| a.join("packages/pantheon").is_dir())
            .expect("workspace root")
            .join("packages/pantheon/agents/shared/sisyphus.md"),
    )?;
    let core_head = core
        .lines()
        .find(|line| line.len() > 20)
        .unwrap_or_default();
    assert!(
        prompt.contains(core_head),
        "prompt must contain shared/sisyphus.md; looked for {core_head:?}"
    );
    assert!(
        !prompt.contains("{{"),
        "prompt must be fully interpolated; got:\n{prompt}"
    );

    worker.abort();
    let _ = worker.await;
    Ok(())
}

/// A variable the agent declares but nobody supplies must produce an error
/// that names the variable, not a bare template failure. The worker is never a
/// TTY, so there is nothing to prompt.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_reports_required_agent_variable_that_was_never_supplied() -> Result<()> {
    require_nextest();

    let Some(server) = spawn_nats_server().await? else {
        eprintln!("skipping: nats-server not available");
        return Ok(());
    };

    let env = TestEnv::new(server)?;
    env.write_package_agent()?;
    // Same agent as the happy path, but started without `repo`.
    let worker = env
        .spawn_worker_with("worker-missing-variable", fixed_reply_call_fn("stub"), &[])
        .await?;

    let agent = format!("{PACKAGE_NAME}/{PACKAGE_AGENT_STEM}");
    let turn = env.run_turn(&agent, "agent-missing-variable").await?;

    let error = turn
        .error
        .context("turn must report the unsupplied variable")?;
    assert!(
        error.contains("repo"),
        "error must name the missing variable, got: {error}"
    );
    assert!(
        error.contains("Repository under work"),
        "error should carry the variable's description so the operator knows \
         what to supply, got: {error}"
    );

    worker.abort();
    let _ = worker.await;
    Ok(())
}

/// A prompt the worker cannot render must end the turn with a visible error
/// instead of leaving the client waiting.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_reports_template_error_to_client() -> Result<()> {
    require_nextest();

    let Some(server) = spawn_nats_server().await? else {
        eprintln!("skipping: nats-server not available");
        return Ok(());
    };

    let env = TestEnv::new(server)?;
    env.write_undefined_variable_agent()?;
    let worker = env.spawn_worker("worker-template-error").await?;

    let session_id = "agent-template-error";
    let turn = env.run_turn(BROKEN_AGENT, session_id).await?;

    let error = turn
        .error
        .context("turn must report the template failure")?;
    assert!(
        error.contains("never_defined"),
        "error must name the undefined variable, got: {error}"
    );
    assert_eq!(
        turn.response, None,
        "a failed turn must not report an assistant response"
    );

    let entries = load_entries(&env.jetstream().await?, session_id).await?;
    let messages = error_entry_messages(&entries);
    assert_eq!(
        messages.len(),
        1,
        "worker must record exactly one durable Error entry, got {messages:?}"
    );
    assert!(
        messages[0].contains("never_defined"),
        "durable Error entry must carry the failure text, got: {}",
        messages[0]
    );

    worker.abort();
    let _ = worker.await;
    Ok(())
}

/// An agent whose file is present but unloadable must fail the turn. Falling
/// back to the worker's own config would answer as a different agent than the
/// one the client asked for, without saying so.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_reports_unloadable_agent_instead_of_falling_back() -> Result<()> {
    require_nextest();

    let Some(server) = spawn_nats_server().await? else {
        eprintln!("skipping: nats-server not available");
        return Ok(());
    };

    let env = TestEnv::new(server)?;
    env.write_unloadable_agent()?;
    let worker = env.spawn_worker("worker-unloadable-agent").await?;

    let session_id = "agent-unloadable";
    let turn = env.run_turn(UNLOADABLE_AGENT, session_id).await?;

    let error = turn
        .error
        .context("turn must report that the agent could not be loaded")?;
    assert!(
        error.contains(UNLOADABLE_AGENT),
        "error must name the agent, got: {error}"
    );
    assert_eq!(
        turn.response, None,
        "the worker must not answer as its own fallback agent"
    );

    worker.abort();
    let _ = worker.await;
    Ok(())
}

/// A worker that dies mid-turn writes nothing at all. The client's lease
/// watchdog must notice the holder is gone and end the turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_ends_turn_when_worker_vanishes_without_writing() -> Result<()> {
    require_nextest();

    let Some(server) = spawn_nats_server().await? else {
        eprintln!("skipping: nats-server not available");
        return Ok(());
    };

    let env = TestEnv::new(server)?;
    env.write_file_backed_variable_agent()?;

    // Stand in for a worker that claims the session and then dies: hold the
    // lease, never answer, then let it go without writing a barrier.
    let session_id = "agent-orphaned-turn";
    let lease = NatsSessionLease::acquire(NatsLeaseAcquireParams {
        jetstream: env.jetstream().await?,
        session_id,
        worker_id: "worker-that-dies".to_string(),
        generation: 1,
        config: NatsLeaseConfig::default(),
        session_index: None,
    })
    .await?
    .context("test must be able to hold the session lease")?;

    let turn = tokio::spawn({
        let server_url = env.server.url().to_string();
        let session_id = session_id.to_string();
        async move {
            let client = async_nats::connect(&server_url).await?;
            let jetstream = async_nats::jetstream::new(client.clone());
            let thin = ThinClientSession::new(
                ThinClientConfig {
                    cluster: "local".to_string(),
                    agent: FILE_VARIABLE_AGENT.to_string(),
                    session_id: Some(session_id),
                },
                client,
                jetstream,
                create_abort_signal(),
            )
            .await?;
            thin.run_turn("hello", Arc::new(NullSink), None).await
        }
    });

    // Let the client observe the lease before it disappears; the watchdog only
    // trips on a holder it has actually seen, and it polls every two seconds.
    tokio::time::sleep(Duration::from_secs(3)).await;
    lease.release().await?;

    let result = tokio::time::timeout(TURN_TIMEOUT, turn)
        .await
        .context("turn stalled after its worker vanished")???;

    let error = result
        .error
        .context("turn must report that the worker stopped")?;
    assert!(
        error.contains("stopped without answering"),
        "error must explain the worker vanished, got: {error}"
    );
    Ok(())
}
