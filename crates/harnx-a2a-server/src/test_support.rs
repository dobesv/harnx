//! Isolated configuration fixtures; nextest runs each test in its own process.

use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};
use tempfile::TempDir;

pub(crate) struct EnvGuard {
    name: &'static str,
    previous: Option<OsString>,
}

impl EnvGuard {
    pub(crate) fn set(name: &'static str, value: Option<&std::ffi::OsStr>) -> Self {
        let previous = std::env::var_os(name);
        match value {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
        Self { name, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var(self.name, value),
            None => std::env::remove_var(self.name),
        }
    }
}

pub(crate) struct TestConfigSandbox {
    // Restore environment before deleting its directories.
    _env: Vec<EnvGuard>,
    config_dir: PathBuf,
    _root: TempDir,
}

impl TestConfigSandbox {
    pub(crate) fn new() -> Self {
        harnx_core::require_nextest();
        let root = tempfile::tempdir().expect("create config sandbox");
        let mut env = Vec::new();
        for (name, subdir) in [
            ("HARNX_CONFIG_DIR", "config"),
            ("HARNX_DATA_DIR", "data"),
            ("HARNX_STATE_DIR", "state"),
        ] {
            let path = root.path().join(subdir);
            fs::create_dir_all(&path).unwrap();
            env.push(EnvGuard::set(name, Some(path.as_os_str())));
        }
        for name in ["HARNX_CONFIG_FILE", "HARNX_NATS_SERVER"] {
            env.push(EnvGuard::set(name, None));
        }
        let config = root.path().join("config");
        fs::create_dir_all(config.join("clients")).unwrap();
        fs::write(config.join("config.yaml"), "model: openai:gpt-4o\n").unwrap();
        fs::write(
            config.join("clients/openai.yaml"),
            "type: openai\napi_key: sk-test\nmodels:\n  - name: gpt-4o\n    type: chat\n    max_input_tokens: 4096\n",
        ).unwrap();
        Self {
            _env: env,
            config_dir: config,
            _root: root,
        }
    }

    pub(crate) fn config_dir(&self) -> &Path {
        // HARNX_CONFIG_DIR may be temporarily overridden by tests.
        &self.config_dir
    }

    pub(crate) fn write_agent(&self, name: &str, description: &str, prompt: &str) {
        self.write_agent_with_front_matter(name, &format!("description: {description}"), prompt);
    }

    pub(crate) fn write_agent_with_front_matter(
        &self,
        name: &str,
        front_matter: &str,
        prompt: &str,
    ) {
        let path = harnx_runtime::config::Config::agent_file(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        // A leading slash on the model client means top-level, even in packages.
        let model = if name.contains('/') {
            "/openai:gpt-4o"
        } else {
            "openai:gpt-4o"
        };
        fs::write(
            path,
            format!("---\nmodel: {model}\n{front_matter}\n---\n{prompt}\n"),
        )
        .unwrap();
    }

    pub(crate) fn write_cluster(&self, name: &str) {
        let dir = self.config_dir().join("nats_servers");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(format!("{name}.yaml")),
            "url: nats://127.0.0.1:4222\n",
        )
        .unwrap();
    }
}

use a2a_lf::{
    A2AError, AgentCard, CancelTaskRequest, DeleteTaskPushNotificationConfigRequest,
    GetExtendedAgentCardRequest, GetTaskPushNotificationConfigRequest, GetTaskRequest,
    ListTaskPushNotificationConfigsRequest, ListTaskPushNotificationConfigsResponse,
    ListTasksRequest, ListTasksResponse, SendMessageRequest, SendMessageResponse, StreamResponse,
    SubscribeToTaskRequest, Task, TaskPushNotificationConfig,
};
use a2a_server_lf::{handler::RequestHandler, middleware::ServiceParams};
use async_trait::async_trait;
use futures::stream::BoxStream;

pub(crate) struct RoutingHandler;

fn not_implemented() -> A2AError {
    A2AError::unsupported_operation("A2A agent execution is not implemented yet")
}

#[async_trait]
impl RequestHandler for RoutingHandler {
    async fn send_message(
        &self,
        _params: &ServiceParams,
        _req: SendMessageRequest,
    ) -> Result<SendMessageResponse, A2AError> {
        Err(not_implemented())
    }

    async fn send_streaming_message(
        &self,
        _params: &ServiceParams,
        _req: SendMessageRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        Err(not_implemented())
    }

    async fn get_task(
        &self,
        _params: &ServiceParams,
        _req: GetTaskRequest,
    ) -> Result<Task, A2AError> {
        Err(not_implemented())
    }

    async fn list_tasks(
        &self,
        _params: &ServiceParams,
        _req: ListTasksRequest,
    ) -> Result<ListTasksResponse, A2AError> {
        Err(not_implemented())
    }

    async fn cancel_task(
        &self,
        _params: &ServiceParams,
        _req: CancelTaskRequest,
    ) -> Result<Task, A2AError> {
        Err(not_implemented())
    }

    async fn subscribe_to_task(
        &self,
        _params: &ServiceParams,
        _req: SubscribeToTaskRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        Err(not_implemented())
    }

    async fn create_push_config(
        &self,
        _params: &ServiceParams,
        _req: TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        Err(A2AError::push_notification_not_supported())
    }

    async fn get_push_config(
        &self,
        _params: &ServiceParams,
        _req: GetTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        Err(A2AError::push_notification_not_supported())
    }

    async fn list_push_configs(
        &self,
        _params: &ServiceParams,
        _req: ListTaskPushNotificationConfigsRequest,
    ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
        Err(A2AError::push_notification_not_supported())
    }

    async fn delete_push_config(
        &self,
        _params: &ServiceParams,
        _req: DeleteTaskPushNotificationConfigRequest,
    ) -> Result<(), A2AError> {
        Err(A2AError::push_notification_not_supported())
    }

    async fn get_extended_agent_card(
        &self,
        _params: &ServiceParams,
        _req: GetExtendedAgentCardRequest,
    ) -> Result<AgentCard, A2AError> {
        Err(not_implemented())
    }
}
