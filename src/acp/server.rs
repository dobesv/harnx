use super::NestedAcpEvent;
use agent_client_protocol::{self as acp, Client as _};
use std::{cell::RefCell, collections::HashMap, rc::Rc};
use uuid::Uuid;

use crate::client::{Client, SseEvent, SseHandler};
use crate::config::{GlobalConfig, Input};
use crate::tool::{ToolCall, ToolResult};
use crate::ui_output::UiOutputEventKind;
use crate::utils::{wait_abort_signal, AbortSignal, AbortSignalInner};

use anyhow::bail;
use serde_json::{json, Value};
use tokio::sync::mpsc::unbounded_channel;

const MAX_TOOL_CALL_ROUNDS: u32 = 100;
const MAX_POST_TOOL_LIMIT_ROUNDS: u32 = 1;

pub struct HarnxAgent {
    agent_name: String,
    config: GlobalConfig,
    sessions: RefCell<HashMap<String, HarnxSession>>,
    connection: RefCell<Option<Rc<acp::AgentSideConnection>>>,
}

#[derive(Clone)]
struct HarnxSession {
    id: String,
    abort_signal: AbortSignal,
}

impl HarnxAgent {
    pub fn new(agent_name: String, config: GlobalConfig) -> Self {
        Self {
            agent_name,
            config,
            sessions: RefCell::new(HashMap::new()),
            connection: RefCell::new(None),
        }
    }

    pub fn set_connection(&self, conn: Rc<acp::AgentSideConnection>) {
        self.connection.replace(Some(conn));
    }

    async fn send_text_chunk(&self, session_id: &str, text: &str) -> acp::Result<()> {
        let connection = self.connection.borrow().clone();
        if let Some(connection) = connection {
            let notification = acp::SessionNotification::new(
                acp::SessionId::new(session_id.to_string()),
                acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                    text.to_string().into(),
                )),
            );
            connection.session_notification(notification).await?;
        }
        Ok(())
    }

    async fn send_usage(
        &self,
        session_id: &str,
        input_tokens: u64,
        output_tokens: u64,
        cached_tokens: u64,
    ) -> acp::Result<()> {
        if input_tokens == 0 && output_tokens == 0 {
            return Ok(());
        }
        let connection = self.connection.borrow().clone();
        if let Some(connection) = connection {
            // Include the harnx session name (human-readable) when available,
            // falling back to the ACP session ID.
            let session_name = self
                .config
                .read()
                .session
                .as_ref()
                .map(|s| s.name().to_string())
                .unwrap_or_default();
            let mut meta = serde_json::Map::new();
            meta.insert(
                "harnx:usage".to_string(),
                json!({
                    "agent": self.agent_name,
                    "session": session_name,
                    "input_tokens": input_tokens,
                    "output_tokens": output_tokens,
                    "cached_tokens": cached_tokens,
                }),
            );
            let update = acp::SessionInfoUpdate::new().meta(meta);
            let notification = acp::SessionNotification::new(
                acp::SessionId::new(session_id.to_string()),
                acp::SessionUpdate::SessionInfoUpdate(update),
            );
            connection.session_notification(notification).await?;
        }
        Ok(())
    }

    async fn execute_llm_streaming(
        &self,
        session_id: &str,
        input: &Input,
        client: &dyn Client,
        abort_signal: &AbortSignal,
    ) -> Result<(String, Option<String>, Vec<ToolCall>), acp::Error> {
        let (tx, mut rx) = unbounded_channel();
        let mut handler = SseHandler::new(tx, abort_signal.clone());

        let connection = self.connection.borrow().clone();
        let sid = session_id.to_string();

        let (send_ret, _) = tokio::join!(
            client.chat_completions_streaming(input, &mut handler),
            async {
                while let Some(event) = rx.recv().await {
                    match event {
                        SseEvent::Text(chunk) => {
                            if let Some(ref conn) = connection {
                                let notification = acp::SessionNotification::new(
                                    acp::SessionId::new(sid.clone()),
                                    acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                                        chunk.into(),
                                    )),
                                );
                                if let Err(e) = conn.session_notification(notification).await {
                                    warn!("ACP streaming notification failed: {e}");
                                }
                            }
                        }
                        SseEvent::Done => break,
                    }
                }
            }
        );

        send_ret.map_err(|e| acp::Error::new(-32603, e.to_string()))?;

        let (text, thought, tool_calls, usage) = handler.take();

        // Send token usage stats to the parent via SessionInfoUpdate._meta
        let _ = self
            .send_usage(
                session_id,
                usage.input_tokens,
                usage.output_tokens,
                usage.cached_tokens,
            )
            .await;

        Ok((text, thought, tool_calls))
    }

    async fn execute_llm_non_streaming(
        &self,
        session_id: &str,
        input: &Input,
        client: &dyn Client,
        abort_signal: &AbortSignal,
    ) -> Result<(String, Option<String>, Vec<ToolCall>), acp::Error> {
        let output = tokio::select! {
            result = client.chat_completions(input.clone()) => {
                result.map_err(|e| acp::Error::new(-32603, e.to_string()))?
            }
            _ = wait_abort_signal(abort_signal) => {
                return Ok((String::new(), None, vec![]));
            }
        };

        if let Some(thought) = &output.thought {
            self.send_text_chunk(session_id, &format!("<think>\n{}\n</think>\n", thought))
                .await?;
        }
        if !output.text.is_empty() {
            self.send_text_chunk(session_id, &output.text).await?;
        }

        // Send token usage stats to the parent via SessionInfoUpdate._meta
        let _ = self
            .send_usage(
                session_id,
                output.input_tokens.unwrap_or(0),
                output.output_tokens.unwrap_or(0),
                output.cached_tokens.unwrap_or(0),
            )
            .await;

        Ok((output.text, output.thought, output.tool_calls))
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Agent for HarnxAgent {
    async fn initialize(
        &self,
        args: acp::InitializeRequest,
    ) -> acp::Result<acp::InitializeResponse> {
        Ok(acp::InitializeResponse::new(args.protocol_version)
            .agent_capabilities(acp::AgentCapabilities::new())
            .agent_info(
                acp::Implementation::new("harnx", env!("CARGO_PKG_VERSION"))
                    .title(self.agent_name.clone()),
            ))
    }

    async fn authenticate(
        &self,
        _args: acp::AuthenticateRequest,
    ) -> acp::Result<acp::AuthenticateResponse> {
        Ok(acp::AuthenticateResponse::default())
    }

    async fn new_session(
        &self,
        _args: acp::NewSessionRequest,
    ) -> acp::Result<acp::NewSessionResponse> {
        let session_id = Uuid::new_v4().to_string();
        {
            let mut config = self.config.write();
            if config.session.is_some() {
                config
                    .exit_session()
                    .map_err(|e| acp::Error::new(-32603, format!("Failed to exit session: {e}")))?;
            }
            config
                .use_session(Some(&session_id))
                .map_err(|e| acp::Error::new(-32603, format!("Failed to create session: {e}")))?;
        }
        let session = HarnxSession {
            id: session_id.clone(),
            abort_signal: AbortSignalInner::new(),
        };
        self.sessions
            .borrow_mut()
            .insert(session_id.clone(), session);
        Ok(acp::NewSessionResponse::new(acp::SessionId::new(
            session_id,
        )))
    }

    async fn prompt(&self, args: acp::PromptRequest) -> acp::Result<acp::PromptResponse> {
        let session_key = args.session_id.0.to_string();
        let prompt_text: String = args
            .prompt
            .iter()
            .map(content_block_to_text)
            .collect::<Vec<_>>()
            .join("\n");

        let abort_signal = {
            let sessions = self.sessions.borrow();
            let session = sessions
                .get(session_key.as_str())
                .ok_or_else(acp::Error::invalid_params)?;
            session.abort_signal.reset();
            session.abort_signal.clone()
        };

        {
            let mut config = self.config.write();
            let active_session_name = config.session.as_ref().map(|s| s.name().to_string());
            if active_session_name.as_deref() != Some(session_key.as_str()) {
                if config.session.is_some() {
                    config.exit_session().map_err(|e| {
                        acp::Error::new(-32603, format!("Failed to exit session: {e}"))
                    })?;
                }
                config
                    .use_session(Some(&session_key))
                    .map_err(|e| acp::Error::new(-32603, format!("Failed to load session: {e}")))?;
            }
        }

        let agent = self
            .config
            .read()
            .retrieve_agent(&self.agent_name)
            .map_err(|e| acp::Error::new(-32603, format!("Failed to retrieve agent: {e}")))?;

        let mut input = Input::from_str(&self.config, &prompt_text, None);
        input.set_agent(agent);
        let client = input
            .create_client()
            .map_err(|e| acp::Error::new(-32603, format!("Failed to create client: {e}")))?;

        let mut round = 0u32;
        loop {
            if abort_signal.aborted() {
                return Ok(acp::PromptResponse::new(acp::StopReason::EndTurn));
            }

            let (output, thought, tool_calls) = if input.stream() {
                self.execute_llm_streaming(&session_key, &input, client.as_ref(), &abort_signal)
                    .await?
            } else {
                self.execute_llm_non_streaming(&session_key, &input, client.as_ref(), &abort_signal)
                    .await?
            };

            let config = self.config.clone();
            let input_for_save = input.clone();
            let output_for_save = output.clone();
            let thought_for_save = thought.clone();
            tokio::task::spawn_blocking(move || {
                let mut config = config.write();
                config.save_message(
                    &input_for_save,
                    &output_for_save,
                    thought_for_save.as_deref(),
                )
            })
            .await
            .map_err(|e| acp::Error::new(-32603, format!("Failed to join save task: {e}")))?
            .map_err(|e| acp::Error::new(-32603, format!("Failed to save message: {e}")))?;

            if tool_calls.is_empty() {
                return Ok(acp::PromptResponse::new(acp::StopReason::EndTurn));
            }

            round += 1;
            let tool_results = if round > MAX_TOOL_CALL_ROUNDS {
                // If the LLM keeps trying to call tools even though we told them they hit the limit, abort
                if round > MAX_TOOL_CALL_ROUNDS + MAX_POST_TOOL_LIMIT_ROUNDS {
                    return Ok(acp::PromptResponse::new(acp::StopReason::EndTurn));
                }

                tool_calls
                    .into_iter()
                    .map(|call| {
                        ToolResult::new(
                            call,
                            json!({
                                "error": "maximum tool call rounds exceeded",
                                "action": "Provide your final answer to the user now. Summarize what you accomplished and any remaining work.",
                                "guidance": "Explain that this session hit the tool call limit. If more tool use is needed, ask the user to continue in a new session or narrow the request."
                            }),
                        )
                    })
                    .collect()
            } else {
                let acp_manager = self.config.read().acp_manager.clone();
                let result = if let Some(ref manager) = acp_manager {
                    let (mut chunk_rx, subscription_id) = manager.subscribe_chunks().await;
                    let connection = self.connection.borrow().clone();
                    let sid = session_key.clone();

                    let forward_task = tokio::task::spawn_local(async move {
                        while let Some(chunk) = chunk_rx.recv().await {
                            if let Some(ref conn) = connection {
                                let update = match chunk {
                                    NestedAcpEvent::Ui(event) => {
                                        nested_ui_event_to_session_update(event).await
                                    }
                                    NestedAcpEvent::Text(text) => {
                                        Some(acp::SessionUpdate::AgentMessageChunk(
                                            acp::ContentChunk::new(text.into()),
                                        ))
                                    }
                                };

                                if let Some(update) = update {
                                    let notification = acp::SessionNotification::new(
                                        acp::SessionId::new(sid.clone()),
                                        update,
                                    );
                                    if let Err(e) = conn.session_notification(notification).await {
                                        warn!("ACP nested streaming notification failed: {e}");
                                    }
                                }
                            }
                        }
                    });

                    let result =
                        eval_tool_calls_async(&self.config, tool_calls, &abort_signal).await;

                    manager.unsubscribe_chunks(subscription_id).await;
                    let _ = forward_task.await;

                    result
                } else {
                    eval_tool_calls_async(&self.config, tool_calls, &abort_signal).await
                };

                match result {
                    Ok(results) => results,
                    Err(e) => {
                        self.send_text_chunk(&session_key, &format!("\n[Tool error: {e:#}]"))
                            .await?;
                        return Ok(acp::PromptResponse::new(acp::StopReason::EndTurn));
                    }
                }
            };

            input = input.merge_tool_results(output, thought, tool_results);
        }
    }

    async fn cancel(&self, args: acp::CancelNotification) -> acp::Result<()> {
        let session_id = args.session_id.0;
        let sessions = self.sessions.borrow();
        let session = sessions
            .get(session_id.as_ref())
            .ok_or_else(acp::Error::invalid_params)?;
        session.abort_signal.set_ctrlc();
        Ok(())
    }
}

fn content_block_to_text(content: &acp::ContentBlock) -> String {
    match content {
        acp::ContentBlock::Text(text) => text.text.clone(),
        acp::ContentBlock::ResourceLink(link) => link.uri.to_string(),
        acp::ContentBlock::Image(_) => "<image>".to_string(),
        acp::ContentBlock::Audio(_) => "<audio>".to_string(),
        acp::ContentBlock::Resource(_) => "<resource>".to_string(),
        _ => String::new(),
    }
}

// Async alternative to `crate::tool::eval_tool_calls` that works on
// single-threaded runtimes (the sync version uses `block_in_place` which
// panics on `current_thread`). Skips CLI hooks since they are designed
// for interactive terminal use.
async fn nested_ui_event_to_session_update(
    event: crate::ui_output::UiOutputEvent,
) -> Option<acp::SessionUpdate> {
    match event.kind {
        UiOutputEventKind::LlmText(text) | UiOutputEventKind::TranscriptText { text } => Some(
            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(text.into())),
        ),
        UiOutputEventKind::ToolResultText { .. } => None,
        UiOutputEventKind::AcpThought { text } => Some(acp::SessionUpdate::AgentThoughtChunk(
            acp::ContentChunk::new(text.into()),
        )),
        UiOutputEventKind::StructuredBlock { title, body } => Some(acp::SessionUpdate::ToolCall(
            acp::ToolCall::new(title, body.unwrap_or_default()),
        )),
        UiOutputEventKind::StatusLine { text } => Some(acp::SessionUpdate::ToolCallUpdate(
            acp::ToolCallUpdate::new("status", acp::ToolCallUpdateFields::new().title(text)),
        )),
        UiOutputEventKind::Plan { entries } => {
            let mapped_entries = entries
                .into_iter()
                .map(|entry| {
                    let status = match entry.status.as_str() {
                        "completed" => acp::PlanEntryStatus::Completed,
                        "in_progress" => acp::PlanEntryStatus::InProgress,
                        _ => acp::PlanEntryStatus::Pending,
                    };
                    acp::PlanEntry::new(entry.content, acp::PlanEntryPriority::Medium, status)
                })
                .collect();
            Some(acp::SessionUpdate::Plan(acp::Plan::new(mapped_entries)))
        }
        UiOutputEventKind::McpToolInvocation {
            tool_name,
            input_yaml,
        } => Some(acp::SessionUpdate::ToolCall(acp::ToolCall::new(
            tool_name,
            input_yaml.unwrap_or_default(),
        ))),
        UiOutputEventKind::LlmFinal { output, .. } => Some(acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new(output.into()),
        )),
        UiOutputEventKind::LlmError(err) => Some(acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new(format!("[error] {err}").into()),
        )),
        UiOutputEventKind::Usage { .. } => None,
    }
}

async fn eval_tool_calls_async(
    config: &GlobalConfig,
    mut calls: Vec<ToolCall>,
    abort_signal: &AbortSignal,
) -> anyhow::Result<Vec<ToolResult>> {
    let mut output = vec![];
    if calls.is_empty() {
        return Ok(output);
    }
    calls = ToolCall::dedup(calls);
    if calls.is_empty() {
        bail!("The request was aborted because an infinite loop of function calls was detected.")
    }

    let mut is_all_null = true;
    for call in calls {
        if abort_signal.aborted() {
            bail!("Tool execution cancelled");
        }
        let result = eval_mcp_async(config, &call, abort_signal).await;
        match result {
            Ok(mut value) => {
                if value.is_null() {
                    value = json!("DONE");
                } else {
                    is_all_null = false;
                }
                output.push(ToolResult::new(call, value));
            }
            Err(err) => {
                return Err(err);
            }
        }
    }
    if is_all_null {
        output = vec![];
    }
    Ok(output)
}

async fn eval_mcp_async(
    config: &GlobalConfig,
    call: &ToolCall,
    abort_signal: &AbortSignal,
) -> anyhow::Result<Value> {
    let json_data = if call.arguments.is_null() {
        Value::Null
    } else if call.arguments.is_object() {
        call.arguments.clone()
    } else if let Some(arguments) = call.arguments.as_str() {
        serde_json::from_str(arguments).map_err(|_| {
            anyhow::anyhow!(
                "The call '{}' has invalid arguments: {arguments}",
                call.name
            )
        })?
    } else {
        bail!(
            "The call '{}' has invalid arguments: {}",
            call.name,
            call.arguments
        );
    };

    let acp_manager = config.read().acp_manager.clone();
    if let Some(manager) = acp_manager {
        if manager.find_client_for_tool(&call.name).is_some() {
            return tokio::select! {
                result = manager.call_tool(&call.name, json_data) => result,
                _ = wait_abort_signal(abort_signal) => bail!("ACP tool call cancelled"),
            };
        }
    }

    let mcp_manager = config.read().mcp_manager.clone();
    let manager = match mcp_manager {
        Some(m) => m,
        None => bail!("No tool provider configured for '{}'", call.name),
    };

    tokio::select! {
        result = manager.call_tool(&call.name, json_data) => result,
        _ = wait_abort_signal(abort_signal) => bail!("MCP tool call cancelled"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        client::{ClientConfig, Model, ModelType, TestStateGuard},
        config::{Config, CREATE_TITLE_AGENT},
        test_utils::{MockClient, MockTurnBuilder},
    };
    use agent_client_protocol::Agent;
    use std::{
        cell::RefCell,
        pin::Pin,
        rc::Rc,
        sync::Arc,
        task::{Context as TaskContext, Poll},
    };
    use tokio::io::{AsyncRead as TokioAsyncRead, AsyncWrite as TokioAsyncWrite, ReadBuf};
    use tokio::task::LocalSet;
    use tokio::time::{timeout, Duration};

    struct TokioCompat<T> {
        inner: T,
    }

    impl<T> TokioCompat<T> {
        fn new(inner: T) -> Self {
            Self { inner }
        }
    }

    impl<T: TokioAsyncRead + Unpin> futures_util::io::AsyncRead for TokioCompat<T> {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut TaskContext<'_>,
            buf: &mut [u8],
        ) -> Poll<std::io::Result<usize>> {
            let mut read_buf = ReadBuf::new(buf);
            match Pin::new(&mut self.inner).poll_read(cx, &mut read_buf) {
                Poll::Ready(Ok(())) => Poll::Ready(Ok(read_buf.filled().len())),
                Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
                Poll::Pending => Poll::Pending,
            }
        }
    }

    impl<T: TokioAsyncWrite + Unpin> futures_util::io::AsyncWrite for TokioCompat<T> {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut TaskContext<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Pin::new(&mut self.inner).poll_write(cx, buf)
        }

        fn poll_flush(
            mut self: Pin<&mut Self>,
            cx: &mut TaskContext<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.inner).poll_flush(cx)
        }

        fn poll_close(
            mut self: Pin<&mut Self>,
            cx: &mut TaskContext<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.inner).poll_shutdown(cx)
        }
    }

    #[derive(Clone)]
    struct TestClient {
        chunks: Rc<RefCell<Vec<String>>>,
    }

    #[async_trait::async_trait(?Send)]
    impl acp::Client for TestClient {
        async fn request_permission(
            &self,
            _args: acp::RequestPermissionRequest,
        ) -> acp::Result<acp::RequestPermissionResponse> {
            Err(acp::Error::method_not_found())
        }

        async fn session_notification(&self, args: acp::SessionNotification) -> acp::Result<()> {
            if let acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk { content, .. }) =
                args.update
            {
                let text = content_block_to_text(&content);
                if !text.is_empty() {
                    self.chunks.borrow_mut().push(text);
                }
            }
            Ok(())
        }
    }

    fn run_local<F: std::future::Future>(future: F) -> F::Output {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build ACP server test runtime");
        let local_set = LocalSet::new();
        local_set.block_on(&rt, future)
    }

    fn test_config() -> GlobalConfig {
        use parking_lot::RwLock;
        use std::sync::Arc;

        let clients: Vec<ClientConfig> = serde_yaml::from_str(
            r#"
- type: openai
  api_key: test-key
  models:
    - name: gpt-4o
      type: chat
      max_input_tokens: 128000
      max_output_tokens: 8192
"#,
        )
        .expect("parse test client config");

        let mut config = Config::default();
        config.clients = clients;
        config.model = Model::retrieve_model(&config, "openai:gpt-4o", ModelType::Chat)
            .expect("load test model");
        config.save_session = Some(true);

        Arc::new(RwLock::new(config))
    }

    #[allow(clippy::type_complexity)]
    fn setup_roundtrip(
        agent_name: &str,
        config: GlobalConfig,
    ) -> (
        acp::ClientSideConnection,
        Rc<RefCell<Vec<String>>>,
        tokio::task::JoinHandle<acp::Result<()>>,
        tokio::task::JoinHandle<acp::Result<()>>,
    ) {
        let agent = Rc::new(HarnxAgent::new(agent_name.to_string(), config));
        let (server_stream, client_stream) = tokio::io::duplex(16 * 1024);
        let (server_reader, server_writer) = tokio::io::split(server_stream);
        let (client_reader, client_writer) = tokio::io::split(client_stream);

        let (server_conn, server_io) = acp::AgentSideConnection::new(
            Rc::clone(&agent),
            TokioCompat::new(server_writer),
            TokioCompat::new(server_reader),
            |future| {
                tokio::task::spawn_local(future);
            },
        );
        agent.set_connection(Rc::new(server_conn));

        let chunks = Rc::new(RefCell::new(Vec::new()));
        let (client_conn, client_io) = acp::ClientSideConnection::new(
            TestClient {
                chunks: Rc::clone(&chunks),
            },
            TokioCompat::new(client_writer),
            TokioCompat::new(client_reader),
            |future| {
                tokio::task::spawn_local(future);
            },
        );

        let server_handle = tokio::task::spawn_local(server_io);
        let client_handle = tokio::task::spawn_local(client_io);

        (client_conn, chunks, server_handle, client_handle)
    }

    #[test]
    fn test_new_session_returns_unique_ids() {
        let config = test_config();
        run_local(async move {
            let agent = HarnxAgent::new("test".to_string(), config);
            let cwd = std::env::current_dir().expect("current dir");

            let resp1 = agent
                .new_session(acp::NewSessionRequest::new(cwd.clone()))
                .await
                .expect("create first session");
            let resp2 = agent
                .new_session(acp::NewSessionRequest::new(cwd))
                .await
                .expect("create second session");
            let session_id1 = resp1.session_id.0.to_string();
            let session_id2 = resp2.session_id.0.to_string();

            assert_ne!(resp1.session_id, resp2.session_id);
            assert!(agent.sessions.borrow().contains_key(session_id1.as_str()));
            assert!(agent.sessions.borrow().contains_key(session_id2.as_str()));
        });
    }

    #[test]
    fn test_cancel_marks_session() {
        let config = test_config();
        run_local(async move {
            let agent = HarnxAgent::new("test".to_string(), config);
            let response = agent
                .new_session(acp::NewSessionRequest::new(
                    std::env::current_dir().expect("current dir"),
                ))
                .await
                .expect("create session");
            let session_id = response.session_id.0.to_string();

            agent
                .cancel(acp::CancelNotification::new(session_id.clone()))
                .await
                .expect("cancel session");

            let sessions = agent.sessions.borrow();
            let session = sessions.get(session_id.as_str()).expect("stored session");
            assert!(session.abort_signal.aborted());
        });
    }

    #[test]
    fn test_cancel_unknown_session_errors() {
        let config = test_config();
        run_local(async move {
            let agent = HarnxAgent::new("test".to_string(), config);

            let result = agent
                .cancel(acp::CancelNotification::new("nonexistent".to_string()))
                .await;

            assert!(result.is_err());
        });
    }

    #[test]
    fn test_acp_server_initialize_handshake() {
        let config = test_config();

        run_local(async move {
            let (client_conn, _chunks, server_handle, client_handle) =
                setup_roundtrip("test", config);

            let response = timeout(
                Duration::from_secs(5),
                client_conn.initialize(
                    acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_info(
                        acp::Implementation::new("test-client", "0.1.0").title("Test Client"),
                    ),
                ),
            )
            .await
            .expect("initialize should not hang")
            .expect("initialize should succeed");

            assert_eq!(response.protocol_version, acp::ProtocolVersion::V1);
            assert!(response.agent_info.is_some());

            server_handle.abort();
            client_handle.abort();
        });
    }

    #[test]
    fn nested_ui_event_maps_to_structured_session_updates() {
        run_local(async {
            let tool_update = nested_ui_event_to_session_update(crate::ui_output::UiOutputEvent {
                kind: UiOutputEventKind::McpToolInvocation {
                    tool_name: "argus_session_prompt".to_string(),
                    input_yaml: Some("message: hello".to_string()),
                },
                source: None,
            })
            .await
            .expect("tool update");

            match tool_update {
                acp::SessionUpdate::ToolCall(call) => {
                    assert!(format!("{:?}", call).contains("argus_session_prompt"));
                    assert!(format!("{:?}", call).contains("message: hello"));
                }
                other => panic!("unexpected tool update: {other:?}"),
            }

            let thought_update =
                nested_ui_event_to_session_update(crate::ui_output::UiOutputEvent {
                    kind: UiOutputEventKind::AcpThought {
                        text: "thinking".to_string(),
                    },
                    source: None,
                })
                .await
                .expect("thought update");
            assert!(matches!(
                thought_update,
                acp::SessionUpdate::AgentThoughtChunk(_)
            ));

            let plan_update = nested_ui_event_to_session_update(crate::ui_output::UiOutputEvent {
                kind: UiOutputEventKind::Plan {
                    entries: vec![crate::ui_output::UiOutputPlanEntry {
                        status: "in_progress".to_string(),
                        content: "delegate to argus".to_string(),
                    }],
                },
                source: None,
            })
            .await
            .expect("plan update");
            assert!(matches!(plan_update, acp::SessionUpdate::Plan(_)));
        });
    }

    #[test]
    fn test_acp_server_new_session_and_prompt_roundtrip() {
        let config = test_config();

        run_local(async move {
            let _guard = TestStateGuard::new(Some(Arc::new(
                MockClient::builder()
                    .add_turn(
                        MockTurnBuilder::new()
                            .add_text_chunk("mock roundtrip response")
                            .build(),
                    )
                    .build(),
            )))
            .await;

            let (client_conn, chunks, server_handle, client_handle) =
                setup_roundtrip(CREATE_TITLE_AGENT, config.clone());

            timeout(
                Duration::from_secs(5),
                client_conn.initialize(
                    acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_info(
                        acp::Implementation::new("test-client", "0.1.0").title("Test Client"),
                    ),
                ),
            )
            .await
            .expect("initialize should not hang")
            .expect("initialize should succeed");

            let session = timeout(
                Duration::from_secs(5),
                client_conn.new_session(acp::NewSessionRequest::new(
                    std::env::current_dir().expect("current dir"),
                )),
            )
            .await
            .expect("new_session should not hang")
            .expect("new_session should succeed");

            assert_eq!(
                config.read().session.as_ref().map(|s| s.name().to_string()),
                Some(session.session_id.to_string())
            );

            let response = timeout(
                Duration::from_secs(5),
                client_conn.prompt(acp::PromptRequest::new(
                    session.session_id.to_string(),
                    vec![acp::ContentBlock::from("hello from client")],
                )),
            )
            .await
            .expect("prompt should not hang")
            .expect("prompt should succeed");

            assert_eq!(response.stop_reason, acp::StopReason::EndTurn);
            let chunks = chunks.borrow();
            assert!(
                chunks.iter().any(|chunk| !chunk.trim().is_empty()),
                "expected prompt roundtrip output to include at least one non-empty chunk, got {:?}",
                *chunks
            );

            let session_path = config.read().session_file(&session.session_id.to_string());
            assert!(
                !session_path.display().to_string().contains("/sessions/_/"),
                "session file should not be written under '_' temp directory: {}",
                session_path.display()
            );
            assert!(
                session_path.exists(),
                "ACP prompt should persist the session to disk at {}",
                session_path.display()
            );

            server_handle.abort();
            client_handle.abort();
        });
    }
}
