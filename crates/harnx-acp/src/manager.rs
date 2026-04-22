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

    pub async fn unsubscribe_chunks(&self, subscription_id: u64) {
        let clients: Vec<_> = self.clients.read().values().cloned().collect();
        for client in clients {
            client.clear_chunk_forwarder(subscription_id).await;
        }
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

                session_prompt_with_abort(&client, session_id, message, tokio::signal::ctrl_c())
                    .await
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
        },
    ]
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) async fn session_prompt_with_abort<Fut>(
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
pub(crate) async fn session_prompt_with_abort_for_test<
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
/// `_allow_fallback_print` is kept in the signature for call-site
/// compatibility but is no longer used — sinks own display.
pub(crate) async fn forward_acp_chunks(
    mut chunk_rx: UnboundedReceiver<NestedAcpEvent>,
    spinner: Option<Spinner>,
    spinner_msg: String,
    _allow_fallback_print: bool,
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

        let (chunk_rx, subscription_id) = self.subscribe_chunks().await;

        // Spawn a spinner only in non-TUI terminal mode.
        let spinner = if is_terminal {
            Some(spawn_spinner(&format!("  {} working…", tool_name)))
        } else {
            None
        };

        // Forward chunks either to TUI output sink or stdout.
        let spinner_clone = spinner.clone();
        let spinner_msg = format!("  {} working…", tool_name);
        let forward_handle = tokio::spawn(forward_acp_chunks(
            chunk_rx,
            spinner_clone,
            spinner_msg,
            is_terminal,
        ));

        // Race the sub-agent call against our abort_signal so Ctrl-C
        // interrupts nested ACP delegations the same way it interrupts
        // MCP tools.
        let tool_name_owned = tool_name.to_string();
        let call_result = tokio::select! {
            result = AcpManager::call_tool(self, &tool_name_owned, arguments) => result,
            _ = wait_abort_signal(abort) => {
                Err(anyhow!("ACP tool call aborted by user"))
            }
        };

        // Tear down: unsubscribe first (closes the channel), then
        // await the forward task.
        self.unsubscribe_chunks(subscription_id).await;
        forward_handle.abort();
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
    async fn forward_acp_chunks_preserves_structured_nested_events() {
        use harnx_core::event::{
            AgentEvent, AgentEventSink, AgentSource, NoticeEvent, ToolEvent, ToolKind,
        };
        use std::sync::Mutex;

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
                title: None,
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

        forward_acp_chunks(rx, None, "test".to_string(), false).await;

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
}
