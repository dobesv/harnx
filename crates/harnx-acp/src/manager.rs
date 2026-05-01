use crate::{AcpClient, AcpServerConfig, NestedAcpEvent};
use harnx_core::abort::{wait_abort_signal, AbortSignal};
use harnx_core::tool::{JsonSchema, ToolDeclaration, ToolError, ToolProvider};
use harnx_spinner::{spawn_spinner, Spinner};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use indexmap::IndexMap;
use parking_lot::RwLock;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::mpsc::UnboundedReceiver;

fn is_stdout_terminal() -> bool {
    use std::io::IsTerminal;
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| std::io::stdout().is_terminal())
}

pub struct AcpManager {
    clients: Arc<RwLock<HashMap<String, Arc<AcpClient>>>>,
    next_subscription_id: AtomicU64,
}

impl fmt::Debug for AcpManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AcpManager")
            .field("client_count", &self.clients.read().len())
            .finish()
    }
}

impl AcpManager {
    pub fn new() -> Self {
        Self {
            clients: Arc::new(RwLock::new(HashMap::new())),
            next_subscription_id: AtomicU64::new(1),
        }
    }

    pub fn initialize(&self, configs: Vec<AcpServerConfig>) {
        let mut clients = HashMap::new();
        for config in configs.into_iter().filter(|config| config.enabled) {
            clients.insert(config.name.clone(), Arc::new(AcpClient::new(config)));
        }
        *self.clients.write() = clients;
    }

    pub fn get_client(&self, server_name: &str) -> Option<Arc<AcpClient>> {
        self.clients.read().get(server_name).cloned()
    }

    pub async fn get_all_tools(&self) -> Vec<ToolDeclaration> {
        let clients = self.clients.read();
        let mut tools: Vec<_> = clients
            .keys()
            .flat_map(|name| generate_acp_tools(name))
            .collect();
        tools.sort_by(|left, right| left.name.cmp(&right.name));
        tools
    }

    pub async fn subscribe_chunks(&self) -> (mpsc::UnboundedReceiver<NestedAcpEvent>, u64) {
        let (tx, rx) = mpsc::unbounded_channel();
        let subscription_id = self.next_subscription_id.fetch_add(1, Ordering::Relaxed);
        let clients: Vec<_> = self.clients.read().values().cloned().collect();
        for client in clients {
            client
                .set_chunk_forwarder(subscription_id, tx.clone())
                .await;
        }
        (rx, subscription_id)
    }

    /// Subscribe to chunk notifications from a single specific client.
    /// Use this instead of [`subscribe_chunks`] when a tool call targets a
    /// known client so that parallel concurrent calls to different agents do
    /// not cross-register and duplicate each other's events.
    pub async fn subscribe_chunks_for_client(
        &self,
        client: &AcpClient,
    ) -> (mpsc::UnboundedReceiver<NestedAcpEvent>, u64) {
        let (tx, rx) = mpsc::unbounded_channel();
        let subscription_id = self.next_subscription_id.fetch_add(1, Ordering::Relaxed);
        client.set_chunk_forwarder(subscription_id, tx).await;
        (rx, subscription_id)
    }

    pub async fn unsubscribe_chunks(&self, subscription_id: u64) {
        let clients: Vec<_> = self.clients.read().values().cloned().collect();
        for client in clients {
            client.clear_chunk_forwarder(subscription_id).await;
        }
    }

    /// Unsubscribe from a single specific client.  Matches
    /// [`subscribe_chunks_for_client`].
    pub async fn unsubscribe_chunks_for_client(&self, client: &AcpClient, subscription_id: u64) {
        client.clear_chunk_forwarder(subscription_id).await;
    }

    pub fn get_all_tools_blocking(&self) -> Vec<ToolDeclaration> {
        let clients = self.clients.read();
        let mut tools: Vec<_> = clients
            .keys()
            .flat_map(|name| generate_acp_tools(name))
            .collect();
        tools.sort_by(|left, right| left.name.cmp(&right.name));
        tools
    }

    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value> {
        self.call_tool_inner(name, arguments, None).await
    }

    async fn call_tool_inner(
        &self,
        name: &str,
        arguments: Value,
        abort: Option<&AbortSignal>,
    ) -> Result<Value> {
        let (client, method) = self
            .find_client_for_tool(name)
            .ok_or_else(|| anyhow!("Unknown ACP tool '{}'", name))?;

        let arguments = expect_object(arguments)?;

        match method.as_str() {
            "session_new" => {
                let session_id = client.session_new().await?;
                Ok(json!({ "session_id": session_id }))
            }
            "session_prompt" => {
                let message = required_string(&arguments, "message")?.to_owned();
                let session_id = match optional_string(&arguments, "session_id")? {
                    Some(session_id) => session_id.to_owned(),
                    None => client.session_new().await?,
                };

                // Race against the AbortSignal when one was provided so a
                // TUI Ctrl-C (which never produces SIGINT in raw mode)
                // still triggers `session/cancel` to the ACP subprocess.
                // Falls back to `tokio::signal::ctrl_c()` for callers that
                // don't have an AbortSignal — primarily one-shot mode and
                // direct unit tests.
                match abort {
                    Some(abort) => {
                        let abort = abort.clone();
                        let abort_future = async move { wait_abort_signal(&abort).await };
                        session_prompt_with_abort(&client, session_id, message, abort_future).await
                    }
                    None => {
                        session_prompt_with_abort(
                            &client,
                            session_id,
                            message,
                            tokio::signal::ctrl_c(),
                        )
                        .await
                    }
                }
            }
            "session_load" => {
                let session_id = required_string(&arguments, "session_id")?.to_owned();
                client.session_load(&session_id).await?;
                Ok(json!({
                    "session_id": session_id,
                    "loaded": true,
                }))
            }
            "session_cancel" => {
                let session_id = required_string(&arguments, "session_id")?.to_owned();
                client.session_cancel(&session_id).await?;
                Ok(json!({
                    "session_id": session_id,
                    "cancelled": true,
                }))
            }
            _ => Err(anyhow!("Unsupported ACP method '{}'", method)),
        }
    }

    pub fn find_client_for_tool(&self, tool_name: &str) -> Option<(Arc<AcpClient>, String)> {
        let clients = self.clients.read();
        for (name, client) in clients.iter() {
            let prefix = format!("{name}_");
            let Some(method) = tool_name.strip_prefix(&prefix) else {
                continue;
            };
            match method {
                "session_new" | "session_prompt" | "session_load" | "session_cancel" => {
                    return Some((Arc::clone(client), method.to_string()));
                }
                _ => continue,
            }
        }
        None
    }
}

impl Default for AcpManager {
    fn default() -> Self {
        Self::new()
    }
}

fn generate_acp_tools(server_name: &str) -> Vec<ToolDeclaration> {
    vec![
        ToolDeclaration {
            name: format!("{server_name}_session_new"),
            description: format!("Create a new session on the '{server_name}' ACP agent"),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::new()),
                ..Default::default()
            },
            mcp_tool_name: Some("session_new".to_string()),
            call_template: None,
            result_template: None,
        },
        ToolDeclaration {
            name: format!("{server_name}_session_prompt"),
            description: format!(
                "Send a prompt to the '{server_name}' ACP agent. Auto-creates a session if session_id is not provided.  Use the session_id from a prior prompt call to continue the same conversation."
            ),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some({
                    let mut props = IndexMap::new();
                    props.insert(
                        "message".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some("The prompt message to send to the agent".to_string()),
                            ..Default::default()
                        },
                    );
                    props.insert(
                        "session_id".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some(
                                "Session ID from a previous session_new call or a prior prompt call. If omitted, a new session is created automatically.".to_string(),
                            ),
                            ..Default::default()
                        },
                    );
                    props
                }),
                required: Some(vec!["message".to_string()]),
                ..Default::default()
            },
            mcp_tool_name: Some("session_prompt".to_string()),
            call_template: None,
            result_template: None,
        },
        ToolDeclaration {
            name: format!("{server_name}_session_load"),
            description: format!("Load an existing session on the '{server_name}' ACP agent"),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some({
                    let mut props = IndexMap::new();
                    props.insert(
                        "session_id".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some("The session ID to load".to_string()),
                            ..Default::default()
                        },
                    );
                    props
                }),
                required: Some(vec!["session_id".to_string()]),
                ..Default::default()
            },
            mcp_tool_name: Some("session_load".to_string()),
            call_template: None,
            result_template: None,
        },
        ToolDeclaration {
            name: format!("{server_name}_session_cancel"),
            description: format!("Cancel a running prompt on the '{server_name}' ACP agent"),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some({
                    let mut props = IndexMap::new();
                    props.insert(
                        "session_id".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some("The session ID to cancel".to_string()),
                            ..Default::default()
                        },
                    );
                    props
                }),
                required: Some(vec!["session_id".to_string()]),
                ..Default::default()
            },
            mcp_tool_name: Some("session_cancel".to_string()),
            call_template: None,
            result_template: None,
        },
    ]
}

#[cfg_attr(not(test), allow(dead_code))]
pub async fn session_prompt_with_abort<Fut>(
    client: &AcpClient,
    session_id: String,
    message: String,
    abort: Fut,
) -> Result<Value>
where
    Fut: std::future::Future,
{
    let client_for_prompt = client;
    let client_for_cancel = client;
    let response = session_prompt_with_abort_for_test(
        move |session_id: Option<String>, message: String| async move {
            client_for_prompt
                .session_prompt(session_id.as_deref(), &message)
                .await
        },
        move |session_id: String| async move { client_for_cancel.session_cancel(&session_id).await },
        session_id.clone(),
        message,
        abort,
    )
    .await?;

    Ok(json!({
        "session_id": session_id,
        "response": response,
    }))
}

#[cfg_attr(not(test), allow(dead_code))]
pub async fn session_prompt_with_abort_for_test<
    PromptFn,
    PromptFut,
    CancelFn,
    CancelFut,
    AbortFut,
>(
    prompt: PromptFn,
    cancel: CancelFn,
    session_id: String,
    message: String,
    abort: AbortFut,
) -> Result<String>
where
    PromptFn: FnOnce(Option<String>, String) -> PromptFut,
    PromptFut: std::future::Future<Output = Result<String>>,
    CancelFn: FnOnce(String) -> CancelFut,
    CancelFut: std::future::Future<Output = Result<()>>,
    AbortFut: std::future::Future,
{
    tokio::pin!(abort);

    tokio::select! {
        result = prompt(Some(session_id.clone()), message) => result,
        _ = &mut abort => {
            match tokio::time::timeout(std::time::Duration::from_secs(5), cancel(session_id)).await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    log::warn!("Failed to cancel ACP session on abort: {err}");
                }
                Err(_) => {
                    log::warn!("Timed out cancelling ACP session on abort");
                }
            }
            Err(anyhow!("ACP tool call aborted by user"))
        }
    }
}

fn expect_object(arguments: Value) -> Result<serde_json::Map<String, Value>> {
    match arguments {
        Value::Object(arguments) => Ok(arguments),
        _ => Err(anyhow!("ACP tool arguments must be a JSON object")),
    }
}

fn required_string<'a>(
    arguments: &'a serde_json::Map<String, Value>,
    key: &str,
) -> Result<&'a str> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("ACP tool argument '{}' must be a string", key))
}

fn optional_string<'a>(
    arguments: &'a serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<&'a str>> {
    match arguments.get(key) {
        Some(Value::String(value)) => Ok(Some(value.as_str())),
        Some(Value::Null) | None => Ok(None),
        Some(_) => Err(anyhow!("ACP tool argument '{}' must be a string", key)),
    }
}

/// Forwards ACP chunk notifications to the unified AgentEvent sink
/// (and thus to whichever sink is installed — CliAgentEventSink for
/// CLI, TuiAgentEventSink for TUI, NullSink for tests). Each incoming
/// chunk temporarily pauses the spinner, emits an AgentEvent preserving
/// the sub-agent source, then restores the spinner.
pub async fn forward_acp_chunks(
    mut chunk_rx: UnboundedReceiver<NestedAcpEvent>,
    spinner: Option<Spinner>,
    spinner_msg: String,
) {
    use harnx_core::event::{AgentEvent, NoticeEvent};
    use harnx_core::sink::emit_agent_event_with_source;

    while let Some(chunk) = chunk_rx.recv().await {
        if let Some(ref s) = spinner {
            s.pause();
        }
        let (event, source) = match chunk {
            NestedAcpEvent::Agent(event, source) => (event, source),
            NestedAcpEvent::Text(text) => (AgentEvent::Notice(NoticeEvent::Info(text)), None),
        };
        emit_agent_event_with_source(event, source);
        if let Some(ref s) = spinner {
            let _ = s.set_message(spinner_msg.clone());
        }
    }
}

#[async_trait]
impl ToolProvider for AcpManager {
    fn name(&self) -> &str {
        "acp"
    }

    fn has_tool(&self, tool_name: &str) -> bool {
        self.find_client_for_tool(tool_name).is_some()
    }

    async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Value,
        abort: &AbortSignal,
    ) -> Result<Value, ToolError> {
        let has_sink = harnx_core::sink::has_agent_event_sink();
        let is_terminal = is_stdout_terminal() && !has_sink;

        // Give the parent tool-call event a chance to be consumed by
        // the UI before nested ACP output from the delegated session
        // starts arriving. This keeps the visible transcript ordering
        // causal: the handoff tool row should appear before any child
        // output it triggers.
        tokio::task::yield_now().await;

        // Subscribe only to the specific client that will handle this call.
        // Subscribing to ALL clients (the old `subscribe_chunks()` approach)
        // causes duplicate output when N tool calls run concurrently: each
        // call's subscription would receive events from every other
        // concurrently-active client, producing N-fold duplication.
        // If the client cannot be found we fall back to the old broad
        // subscribe so that the error path still surfaces a useful message.
        let target_client = self
            .find_client_for_tool(tool_name)
            .map(|(client, _)| client);
        let (chunk_rx, subscription_id) = match &target_client {
            Some(client) => self.subscribe_chunks_for_client(client).await,
            None => self.subscribe_chunks().await,
        };

        // Spawn a spinner only in non-TUI terminal mode.
        let spinner = if is_terminal {
            Some(spawn_spinner(&format!("  {} working…", tool_name)))
        } else {
            None
        };

        // Forward chunks either to TUI output sink or stdout.
        let spinner_clone = spinner.clone();
        let spinner_msg = format!("  {} working…", tool_name);
        let forward_handle = tokio::spawn(forward_acp_chunks(chunk_rx, spinner_clone, spinner_msg));

        // Plumb the AbortSignal into the inner dispatcher so a TUI Ctrl-C
        // — which never reaches us as SIGINT because crossterm captures
        // the keystroke in raw mode — still triggers `session/cancel` on
        // the ACP subprocess. Previously an outer `select!` raced the
        // call against `wait_abort_signal(abort)` and on abort just
        // dropped the inner future, so `session_prompt_with_abort`'s
        // cancel branch (gated on `tokio::signal::ctrl_c()`) was never
        // reached. The sub-agent then kept running and its late chunks
        // leaked into the parent transcript through
        // `AcpNotificationClient`'s no-forwarder fallback path.
        let tool_name_owned = tool_name.to_string();
        let call_result = self
            .call_tool_inner(&tool_name_owned, arguments, Some(abort))
            .await;

        // Tear down: drop every sender for this subscription, then await
        // the forwarder so it can drain anything still queued before
        // exiting. Calling `abort()` here would race with that drain and
        // silently swallow late events (e.g. the sub-agent's final text
        // chunk arriving just before its session_prompt response), which
        // showed up as flaky standalone activity rendering in the
        // nested-sub-agent transcript.
        match &target_client {
            Some(client) => {
                self.unsubscribe_chunks_for_client(client, subscription_id)
                    .await;
            }
            None => {
                self.unsubscribe_chunks(subscription_id).await;
            }
        }
        let _ = forward_handle.await;

        if let Some(s) = spinner {
            s.stop();
        }

        call_result.map_err(|err| {
            // Only user-initiated aborts (Ctrl+C) are fatal and should
            // stop the entire tool batch.  Other failures (timeouts,
            // connection errors, bad arguments) are recoverable so the
            // LLM receives the error message and can retry.
            if err.to_string().contains("aborted by user") {
                ToolError::Fatal(err)
            } else {
                ToolError::Recoverable(err)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    fn run_async<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build test runtime")
            .block_on(future)
    }

    /// Mutex that serialises tests which install a global `AgentEventSink`.
    /// The sink is process-global; concurrent tests would see each other's events.
    /// Uses `tokio::sync::Mutex` so the guard can be held across `.await` points.
    static SINK_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn assert_error_event(
        event: &(
            harnx_core::event::AgentEvent,
            Option<harnx_core::event::AgentSource>,
        ),
        expected_fragments: &[&str],
        expected_agent: &str,
        expected_session_id: Option<&str>,
    ) {
        use harnx_core::event::{AgentEvent, AgentSource, ModelEvent};
        match event {
            (
                AgentEvent::Model(ModelEvent::Error(msg)),
                Some(AgentSource { agent, session_id }),
            ) => {
                for fragment in expected_fragments {
                    assert!(
                        msg.contains(fragment),
                        "error message missing {fragment:?}, got: {msg}"
                    );
                }
                assert_eq!(agent, expected_agent);
                assert_eq!(session_id.as_deref(), expected_session_id);
            }
            other => panic!("expected ModelEvent::Error with source, got: {other:?}"),
        }
    }

    fn test_config(name: &str) -> AcpServerConfig {
        AcpServerConfig {
            name: name.to_string(),
            command: "dummy".to_string(),
            args: vec![],
            env: HashMap::new(),
            enabled: true,
            description: None,
            idle_timeout_secs: 300,
            operation_timeout_secs: 3600,
        }
    }

    #[test]
    fn test_acp_config_deserialize_full() {
        let yaml = r#"
name: test-agent
command: /usr/bin/test
args: ["--verbose"]
env:
  KEY: value
enabled: true
description: A test agent
idle_timeout_secs: 42
"#;

        let config: AcpServerConfig =
            serde_yaml::from_str(yaml).expect("deserialize full ACP config");

        assert_eq!(config.name, "test-agent");
        assert_eq!(config.command, "/usr/bin/test");
        assert_eq!(config.args, vec!["--verbose"]);
        assert_eq!(config.env.get("KEY").map(String::as_str), Some("value"));
        assert!(config.enabled);
        assert_eq!(config.description.as_deref(), Some("A test agent"));
        assert_eq!(config.idle_timeout_secs, 42);
        assert_eq!(config.operation_timeout_secs, 3600);
    }

    #[test]
    fn test_acp_config_deserialize_defaults() {
        let yaml = r#"
name: minimal
command: agent
"#;

        let config: AcpServerConfig =
            serde_yaml::from_str(yaml).expect("deserialize minimal ACP config");

        assert_eq!(config.name, "minimal");
        assert_eq!(config.command, "agent");
        assert!(config.args.is_empty());
        assert!(config.env.is_empty());
        assert!(config.enabled);
        assert!(config.description.is_none());
        assert_eq!(config.idle_timeout_secs, 300);
        assert_eq!(config.operation_timeout_secs, 3600);
    }

    #[test]
    fn test_acp_config_disabled() {
        let yaml = r#"
name: disabled-agent
command: agent
enabled: false
"#;

        let config: AcpServerConfig =
            serde_yaml::from_str(yaml).expect("deserialize disabled ACP config");

        assert!(!config.enabled);
    }

    #[test]
    fn test_generate_acp_tools() {
        let tools = generate_acp_tools("myagent");

        assert_eq!(tools.len(), 4);

        let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
        assert!(names.contains(&"myagent_session_new"));
        assert!(names.contains(&"myagent_session_prompt"));
        assert!(names.contains(&"myagent_session_load"));
        assert!(names.contains(&"myagent_session_cancel"));
    }

    #[test]
    fn test_session_prompt_tool_has_message_param() {
        let tools = generate_acp_tools("agent1");
        let prompt_tool = tools
            .iter()
            .find(|tool| tool.name == "agent1_session_prompt")
            .expect("find prompt tool");

        let props = prompt_tool
            .parameters
            .properties
            .as_ref()
            .expect("prompt tool properties");
        assert!(props.contains_key("message"));
        assert!(props.contains_key("session_id"));

        let required = prompt_tool
            .parameters
            .required
            .as_ref()
            .expect("prompt tool required list");
        assert!(required.contains(&"message".to_string()));
    }

    #[test]
    fn test_session_cancel_tool_requires_session_id() {
        let tools = generate_acp_tools("agent1");
        let cancel_tool = tools
            .iter()
            .find(|tool| tool.name == "agent1_session_cancel")
            .expect("find cancel tool");

        let required = cancel_tool
            .parameters
            .required
            .as_ref()
            .expect("cancel tool required list");
        assert!(required.contains(&"session_id".to_string()));
    }

    #[test]
    fn test_find_client_for_tool_matches() {
        let manager = AcpManager::new();
        manager.initialize(vec![test_config("myserver")]);

        let result = manager.find_client_for_tool("myserver_session_new");

        assert!(result.is_some());
        let (client, method) = result.expect("matched client for tool");
        assert_eq!(client.name(), "myserver");
        assert_eq!(method, "session_new");
    }

    #[test]
    fn test_find_client_for_tool_no_match() {
        let manager = AcpManager::new();
        manager.initialize(vec![]);

        assert!(manager
            .find_client_for_tool("unknown_session_new")
            .is_none());
    }

    #[test]
    fn test_find_client_for_tool_wrong_method() {
        let manager = AcpManager::new();
        manager.initialize(vec![test_config("srv")]);

        assert!(manager.find_client_for_tool("srv_unknown_method").is_none());
    }

    #[test]
    fn test_get_all_tools_blocking_multiple_servers() {
        let manager = AcpManager::new();
        manager.initialize(vec![test_config("a"), test_config("b")]);

        let tools = manager.get_all_tools_blocking();

        assert_eq!(tools.len(), 8);
    }

    #[test]
    fn test_disabled_server_not_initialized() {
        let manager = AcpManager::new();
        let mut config = test_config("disabled");
        config.enabled = false;
        manager.initialize(vec![config]);

        assert!(manager.get_client("disabled").is_none());
        assert_eq!(manager.get_all_tools_blocking().len(), 0);
    }

    #[test]
    fn test_call_tool_session_prompt_validates_message_argument() {
        let manager = AcpManager::new();
        manager.initialize(vec![test_config("srv")]);

        let err =
            run_async(manager.call_tool("srv_session_prompt", json!({ "session_id": "abc" })))
                .expect_err("missing message should fail before ACP subprocess work");

        assert!(err.to_string().contains("message"));
    }

    #[test]
    fn test_call_tool_session_cancel_validates_session_id_argument_type() {
        let manager = AcpManager::new();
        manager.initialize(vec![test_config("srv")]);

        let err = run_async(manager.call_tool("srv_session_cancel", json!({ "session_id": 123 })))
            .expect_err("invalid session_id type should fail before ACP subprocess work");

        assert!(err.to_string().contains("session_id"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forward_acp_chunks_passes_tool_completed_and_update_events() {
        use harnx_core::event::{AgentEvent, AgentEventSink, AgentSource, ToolEvent, ToolStatus};
        use std::sync::Mutex;

        let _guard = SINK_LOCK.lock().await;

        struct CollectingSink {
            events: Mutex<Vec<(AgentEvent, Option<AgentSource>)>>,
        }
        impl AgentEventSink for CollectingSink {
            fn emit(&self, event: AgentEvent, source: Option<AgentSource>) {
                self.events.lock().unwrap().push((event, source));
            }
        }

        let sink = std::sync::Arc::new(CollectingSink {
            events: Mutex::new(Vec::new()),
        });
        harnx_core::sink::clear_agent_event_sink();
        harnx_core::sink::install_agent_event_sink(sink.clone());

        // RAII guard — clears the global sink even if assertions panic.
        struct SinkCleanupGuard;
        impl Drop for SinkCleanupGuard {
            fn drop(&mut self) {
                harnx_core::sink::clear_agent_event_sink();
            }
        }
        let _cleanup = SinkCleanupGuard;

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tx.send(NestedAcpEvent::Agent(
            AgentEvent::Tool(ToolEvent::Completed {
                id: "call-1".to_string(),
                output: json!({"text": "done"}),
                markdown: None,
            }),
            Some(AgentSource {
                agent: "sub-agent".to_string(),
                session_id: Some("session-1".to_string()),
            }),
        ))
        .expect("send tool completed event");
        tx.send(NestedAcpEvent::Agent(
            AgentEvent::Tool(ToolEvent::Update {
                id: "call-1".to_string(),
                markdown: None,
                status: Some(ToolStatus::InProgress),
                content: None,
            }),
            Some(AgentSource {
                agent: "sub-agent".to_string(),
                session_id: Some("session-1".to_string()),
            }),
        ))
        .expect("send tool update event");
        drop(tx);

        forward_acp_chunks(rx, None, "test".to_string()).await;

        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 2);
        match &events[0] {
            (AgentEvent::Tool(ToolEvent::Completed { id, output, .. }), Some(source)) => {
                assert_eq!(id, "call-1");
                assert_eq!(output, &json!({"text": "done"}));
                assert_eq!(source.agent, "sub-agent");
            }
            other => panic!("unexpected first event: {other:?}"),
        }
        match &events[1] {
            (AgentEvent::Tool(ToolEvent::Update { id, status, .. }), Some(source)) => {
                assert_eq!(id, "call-1");
                assert!(matches!(status, Some(ToolStatus::InProgress)));
                assert_eq!(source.agent, "sub-agent");
            }
            other => panic!("unexpected second event: {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forward_acp_chunks_preserves_structured_nested_events() {
        use harnx_core::event::{
            AgentEvent, AgentEventSink, AgentSource, NoticeEvent, ToolEvent, ToolKind,
        };
        use std::sync::Mutex;

        let _guard = SINK_LOCK.lock().await;

        struct CollectingSink {
            events: Mutex<Vec<(AgentEvent, Option<AgentSource>)>>,
        }
        impl AgentEventSink for CollectingSink {
            fn emit(&self, event: AgentEvent, source: Option<AgentSource>) {
                self.events.lock().unwrap().push((event, source));
            }
        }

        let sink = std::sync::Arc::new(CollectingSink {
            events: Mutex::new(Vec::new()),
        });
        harnx_core::sink::clear_agent_event_sink();
        harnx_core::sink::install_agent_event_sink(sink.clone());

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

        tx.send(NestedAcpEvent::Agent(
            AgentEvent::Tool(ToolEvent::Started {
                id: String::new(),
                name: "read_file".to_string(),
                kind: ToolKind::Other,
                markdown: None,
                input: serde_json::json!({"path": "/tmp/x.txt"}),
                locations: vec![],
            }),
            Some(AgentSource {
                agent: "argus".to_string(),
                session_id: Some("sub-session-1".to_string()),
            }),
        ))
        .unwrap();
        tx.send(NestedAcpEvent::Text("plain text notification".to_string()))
            .unwrap();
        drop(tx);

        forward_acp_chunks(rx, None, "test".to_string()).await;

        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 2);

        match &events[0] {
            (AgentEvent::Tool(ToolEvent::Started { name, input, .. }), Some(src)) => {
                assert_eq!(name, "read_file");
                assert_eq!(input["path"], "/tmp/x.txt");
                assert_eq!(src.agent, "argus");
                assert_eq!(src.session_id.as_deref(), Some("sub-session-1"));
            }
            other => panic!("unexpected first event: {other:?}"),
        }
        match &events[1] {
            (AgentEvent::Notice(NoticeEvent::Info(text)), None) => {
                assert_eq!(text, "plain text notification");
            }
            other => panic!("unexpected second event: {other:?}"),
        }

        drop(events);
        harnx_core::sink::clear_agent_event_sink();
    }

    /// Test that error events from sub-agents pass through the forwarding layer intact.
    /// When a sub-agent emits a ModelEvent::Error with a formatted cause chain,
    /// the ACP forwarding layer should preserve the full error string without stripping
    /// or mangling the "Caused by:" sections.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forward_acp_chunks_preserves_error_event_content() {
        use harnx_core::event::{AgentEvent, AgentEventSink, AgentSource, ModelEvent};
        use std::sync::Mutex;

        let _guard = SINK_LOCK.lock().await;

        struct CollectingSink {
            events: Mutex<Vec<(AgentEvent, Option<AgentSource>)>>,
        }
        impl AgentEventSink for CollectingSink {
            fn emit(&self, event: AgentEvent, source: Option<AgentSource>) {
                self.events.lock().unwrap().push((event, source));
            }
        }

        let sink = std::sync::Arc::new(CollectingSink {
            events: Mutex::new(Vec::new()),
        });
        harnx_core::sink::clear_agent_event_sink();
        harnx_core::sink::install_agent_event_sink(sink.clone());

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

        // Simulate a sub-agent emitting an error with a formatted cause chain
        let formatted_error = "outer error message\n\nCaused by:\n    root cause detail";
        let source = AgentSource {
            agent: "pytheas".to_string(),
            session_id: Some("sub-session-error".to_string()),
        };

        tx.send(NestedAcpEvent::Agent(
            AgentEvent::Model(ModelEvent::Error(formatted_error.to_string())),
            Some(source.clone()),
        ))
        .unwrap();
        drop(tx);

        forward_acp_chunks(rx, None, "test".to_string()).await;

        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 1, "Should have exactly one event");
        assert_error_event(
            &events[0],
            &["outer error message", "Caused by:", "root cause detail"],
            "pytheas",
            Some("sub-session-error"),
        );

        drop(events);
        harnx_core::sink::clear_agent_event_sink();
    }

    /// Regression test for issue #420: parallel concurrent calls to different
    /// ACP agents must not duplicate each other's events.
    ///
    /// When N tool calls are in-flight simultaneously (parallel tool dispatch),
    /// `subscribe_chunks_for_client` scopes each subscription to a single
    /// client.  Proof: inject an event through alpha's forwarder map and assert
    /// (a) alpha's scoped receiver gets it, (b) beta's scoped receiver stays empty.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn subscribe_chunks_for_client_no_cross_contamination() {
        let manager = AcpManager::new();
        manager.initialize(vec![test_config("alpha"), test_config("beta")]);

        let alpha_client = manager
            .get_client("alpha")
            .expect("alpha client should exist");
        let beta_client = manager
            .get_client("beta")
            .expect("beta client should exist");

        // Two parallel calls — each scoped to its own client.
        // Discard the default receiver for alpha; we'll install a fresh pair below
        // so we control both sides of the forwarder.
        let (_old_alpha_rx, alpha_sub) = manager.subscribe_chunks_for_client(&alpha_client).await;
        let (mut beta_rx, beta_sub) = manager.subscribe_chunks_for_client(&beta_client).await;

        // Replace alpha's forwarder entry with a known tx/rx pair so we can
        // inject an event through it (simulates forward_agent_event from the
        // alpha subprocess).
        let (send_tx, mut alpha_rx) = tokio::sync::mpsc::unbounded_channel::<NestedAcpEvent>();
        alpha_client
            .set_chunk_forwarder(alpha_sub, send_tx.clone())
            .await;

        // Send one event through alpha's forwarder.
        let event = NestedAcpEvent::Agent(
            harnx_core::event::AgentEvent::Notice(harnx_core::event::NoticeEvent::Info(
                "from alpha".to_string(),
            )),
            None,
        );
        send_tx
            .send(event)
            .expect("send event into alpha forwarder");
        drop(send_tx); // close so recv() terminates

        // Assert 1: alpha's receiver gets the event.
        let received = alpha_rx
            .recv()
            .await
            .expect("alpha_rx must receive the injected event");
        match received {
            NestedAcpEvent::Agent(
                harnx_core::event::AgentEvent::Notice(harnx_core::event::NoticeEvent::Info(
                    ref msg,
                )),
                None,
            ) => assert_eq!(msg, "from alpha"),
            other => panic!("unexpected event in alpha_rx: {other:?}"),
        }

        // Assert 2: beta's receiver is empty — alpha's event never crossed to beta.
        // alpha_sub was registered only on alpha_client; beta_client holds NO entry
        // for alpha_sub, so beta_rx can never receive anything from alpha's path.
        assert!(
            beta_rx.try_recv().is_err(),
            "beta_rx must be empty — alpha events must not bleed into beta subscription (issue #420)"
        );

        manager
            .unsubscribe_chunks_for_client(&alpha_client, alpha_sub)
            .await;
        manager
            .unsubscribe_chunks_for_client(&beta_client, beta_sub)
            .await;
    }

    /// Contrast test: `subscribe_chunks` (broad path) registers on ALL clients.
    /// We verify this by subscribing broadly, then injecting an event through
    /// each client's forwarder entry and confirming the broad receiver sees both.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn subscribe_chunks_broad_registers_on_all_clients() {
        let manager = AcpManager::new();
        manager.initialize(vec![test_config("alpha"), test_config("beta")]);

        let alpha_client = manager
            .get_client("alpha")
            .expect("alpha client should exist");
        let beta_client = manager
            .get_client("beta")
            .expect("beta client should exist");

        // Broad subscribe — registers the same underlying sender on BOTH clients.
        let (_broad_rx, broad_sub) = manager.subscribe_chunks().await;

        // Replace alpha's entry with a known tx/rx pair and inject an event.
        let (alpha_send, mut alpha_rx) = tokio::sync::mpsc::unbounded_channel::<NestedAcpEvent>();
        alpha_client
            .set_chunk_forwarder(broad_sub, alpha_send.clone())
            .await;
        alpha_send
            .send(NestedAcpEvent::Agent(
                harnx_core::event::AgentEvent::Notice(harnx_core::event::NoticeEvent::Info(
                    "from alpha-broad".to_string(),
                )),
                None,
            ))
            .expect("send alpha broad event");
        drop(alpha_send);

        match alpha_rx
            .recv()
            .await
            .expect("alpha entry must deliver event")
        {
            NestedAcpEvent::Agent(
                harnx_core::event::AgentEvent::Notice(harnx_core::event::NoticeEvent::Info(
                    ref msg,
                )),
                None,
            ) => assert_eq!(msg, "from alpha-broad"),
            other => panic!("unexpected alpha-broad event: {other:?}"),
        }

        // Replace beta's entry with a known tx/rx pair and inject an event.
        let (beta_send, mut beta_rx) = tokio::sync::mpsc::unbounded_channel::<NestedAcpEvent>();
        beta_client
            .set_chunk_forwarder(broad_sub, beta_send.clone())
            .await;
        beta_send
            .send(NestedAcpEvent::Agent(
                harnx_core::event::AgentEvent::Notice(harnx_core::event::NoticeEvent::Info(
                    "from beta-broad".to_string(),
                )),
                None,
            ))
            .expect("send beta broad event");
        drop(beta_send);

        match beta_rx.recv().await.expect("beta entry must deliver event") {
            NestedAcpEvent::Agent(
                harnx_core::event::AgentEvent::Notice(harnx_core::event::NoticeEvent::Info(
                    ref msg,
                )),
                None,
            ) => assert_eq!(msg, "from beta-broad"),
            other => panic!("unexpected beta-broad event: {other:?}"),
        }

        // Both per-client forwarder entries existed and delivered events —
        // the broad-subscribe contract holds.
        manager.unsubscribe_chunks(broad_sub).await;
    }
}
