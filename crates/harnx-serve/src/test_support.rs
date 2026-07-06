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
        fs::write(
            root.join("config.yaml"),
            "model: openai:gpt-4o\nsave_session: true\n",
        )
        .expect("write config");
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

    pub fn config(&self) -> Config {
        let prev = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(&self.root).expect("switch cwd");
        let result = futures::executor::block_on(Config::init(WorkingMode::Cmd, false, vec![]));
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
