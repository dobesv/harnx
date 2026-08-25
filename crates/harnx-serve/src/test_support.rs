use harnx_runtime::config::{Config, WorkingMode};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        LazyLock, Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
};

static TEST_CONFIG_DIR_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static TEST_CONFIG_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

pub struct TestConfigSandbox {
    _lock: std::sync::MutexGuard<'static, ()>,
    root: PathBuf,
    data_dir: PathBuf,
    state_dir: PathBuf,
    vars: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl TestConfigSandbox {
    // `new()` mutates global env vars and cwd-sensitive fixture layout; implicit Default would be surprising.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let lock = TEST_CONFIG_DIR_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let root = unique_test_config_dir("test-support");
        let data_dir = root.join("data");
        let state_dir = root.join("state");

        fs::create_dir_all(root.join("clients")).expect("create clients dir");
        fs::create_dir_all(root.join("agents")).expect("create agents dir");
        fs::create_dir_all(&data_dir).expect("create data dir");
        fs::create_dir_all(&state_dir).expect("create state dir");
        fs::write(root.join("config.yaml"), "model: openai:gpt-4o\n").expect("write config");
        fs::write(
            root.join("clients/openai.yaml"),
            concat!(
                "type: openai\n",
                "api_key: sk-test\n",
                "models:\n",
                "  - name: gpt-4o\n",
                "    type: chat\n",
                "    max_input_tokens: 4096\n"
            ),
        )
        .expect("write openai client");

        let vars = vec![
            ("HARNX_CONFIG_DIR", std::env::var_os("HARNX_CONFIG_DIR")),
            ("HARNX_DATA_DIR", std::env::var_os("HARNX_DATA_DIR")),
            ("HARNX_STATE_DIR", std::env::var_os("HARNX_STATE_DIR")),
            // Saved so the `remove_var` below is restored on drop rather than
            // leaking the deletion to the rest of the process.
            ("HARNX_CONFIG_FILE", std::env::var_os("HARNX_CONFIG_FILE")),
        ];
        unsafe {
            std::env::set_var("HARNX_CONFIG_DIR", &root);
            std::env::set_var("HARNX_DATA_DIR", &data_dir);
            std::env::set_var("HARNX_STATE_DIR", &state_dir);
            std::env::remove_var("HARNX_CONFIG_FILE");
        }

        Self {
            _lock: lock,
            root,
            data_dir,
            state_dir,
            vars,
        }
    }

    pub fn write_agent(&self, name: &str, prompt: &str) {
        self.write_agent_with_front_matter(name, "model: openai:gpt-4o", prompt);
    }

    pub fn write_agent_with_front_matter(&self, name: &str, front_matter: &str, prompt: &str) {
        let body = format!("---\n{front_matter}\n---\n{prompt}\n");
        fs::write(self.root.join("agents").join(format!("{name}.md")), body).expect("write agent");
    }

    pub fn write_mock_openai_client(&self, api_base: &str) {
        fs::write(
            self.root.join("config.yaml"),
            "save: false\nstream: true\nclient: mock\nmodel: mock:test\n",
        )
        .expect("write mock global config");
        fs::write(
            self.root.join("clients/mock.yaml"),
            format!(
                "type: openai-compatible\nname: mock\napi_base: {api_base:?}\napi_key: test-key\nmodels:\n  - name: test\n    max_input_tokens: 32000\n    max_output_tokens: 1024\n"
            ),
        )
        .expect("write mock client config");
    }

    pub fn config(&self) -> Config {
        let prev = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(&self.root).expect("switch cwd");
        let result = futures::executor::block_on(Config::init(WorkingMode::Cmd, false));
        std::env::set_current_dir(prev).expect("restore cwd");
        result.expect("load config")
    }
}

impl Drop for TestConfigSandbox {
    fn drop(&mut self) {
        for (key, value) in self.vars.iter().rev() {
            unsafe {
                if let Some(value) = value {
                    std::env::set_var(key, value);
                } else {
                    std::env::remove_var(key);
                }
            }
        }
        let _ = fs::remove_dir_all(&self.data_dir);
        let _ = fs::remove_dir_all(&self.state_dir);
        let _ = fs::remove_dir_all(&self.root);
    }
}

pub struct NatsSessionSeed<'a> {
    pub agent: &'a str,
    pub session_id: &'a str,
    pub messages: &'a [harnx_core::message::Message],
}

/// Seed the local NATS store with a complete session for control-plane tests.
///
/// Returns `false` when the optional `nats-server` test dependency is not
/// installed, allowing NATS-backed tests to follow the workspace convention
/// of skipping on platforms whose CI jobs do not provide the binary.
pub async fn seed_nats_session(config: &Config, seed: NatsSessionSeed<'_>) -> bool {
    use harnx_core::session::SessionLogEntry;
    use harnx_runtime::{
        config::LOCAL_CLUSTER_KEY,
        nats_session_log::NatsSessionLog,
        nats_session_metadata::{SessionMetadata, SessionMetadataStore},
        SessionInitializer,
    };

    if let Err(error) = crate::ensure_frontend_nats_owner().await {
        if error.to_string().contains("nats-server binary not found") {
            eprintln!("skipping NATS-backed harnx-serve test: {error}");
            return false;
        }
        panic!("local NATS owner: {error:#}");
    }
    let mut scoped = config.clone();
    scoped.use_agent_by_name(seed.agent).expect("seed agent");
    let jetstream = config
        .nats_jetstream(LOCAL_CLUSTER_KEY)
        .await
        .expect("local NATS context");
    let metadata_store = SessionMetadataStore::ensure(&jetstream, 1)
        .await
        .expect("session metadata store");
    metadata_store
        .create(&SessionMetadata::new(
            seed.session_id,
            SessionInitializer::from_config(&scoped).expect("session initializer"),
        ))
        .await
        .expect("create session metadata");
    let log = NatsSessionLog::new(jetstream.clone(), seed.session_id.to_string());
    for message in seed.messages {
        log.append_event_async(&SessionLogEntry::Message {
            id: message.id.clone(),
            role: message.role,
            content: message.content.clone(),
            timestamp: message.log_timestamp,
            fence_token: None,
        })
        .await
        .expect("append session message");
    }
    true
}

pub fn unique_test_config_dir(scope: &str) -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time after unix epoch")
        .as_nanos();
    let counter = TEST_CONFIG_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
    canonicalize_for_env(std::env::temp_dir().join(format!(
        "harnx-serve-{scope}-{}-{timestamp}-{counter}",
        std::process::id()
    )))
}

fn canonicalize_for_env(path: PathBuf) -> PathBuf {
    path.parent()
        .and_then(canonicalize_existing_dir)
        .map(|parent| {
            parent.join(
                path.file_name()
                    .expect("temp dir path should include final component"),
            )
        })
        .unwrap_or(path)
}

fn canonicalize_existing_dir(path: &Path) -> Option<PathBuf> {
    fs::canonicalize(path).ok()
}

#[cfg(test)]
pub(crate) async fn wait_for_state(
    handle: &crate::session_actor::SessionHandle,
    description: &str,
    predicate: impl Fn(&crate::session_actor::SessionState) -> bool,
) -> crate::session_actor::SessionInfo {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            handle
                .tx
                .send(crate::session_actor::SessionCommand::Get { reply: reply_tx })
                .await
                .expect("send get");
            let info = reply_rx.await.expect("recv get reply");
            if predicate(&info.state) {
                return info;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for session to become {description}"))
}
