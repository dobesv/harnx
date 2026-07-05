use harnx_runtime::config::{Config, WorkingMode};
use std::{
    fs,
    path::PathBuf,
    sync::{LazyLock, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

static TEST_CONFIG_DIR_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

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
        let root = unique_test_config_dir();
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
        let body = format!("---\nmodel: openai:gpt-4o\n---\n{prompt}\n");
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

fn unique_test_config_dir() -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time after unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "harnx-serve-test-support-{}-{timestamp}",
        std::process::id()
    ))
}
