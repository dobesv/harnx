use agent_client_protocol::{self as acp, Client as _};
use std::{cell::RefCell, collections::HashMap, rc::Rc};
use uuid::Uuid;

use crate::client::{Client, SseEvent, SseHandler};
use crate::config::{GlobalConfig, Input};
use crate::tool::{ToolCall, ToolResult};
use crate::utils::{AbortSignal, AbortSignalInner, wait_abort_signal};

use anyhow::bail;
use serde_json::{json, Value};
use tokio::sync::mpsc::unbounded_channel;

const MAX_TOOL_CALL_ROUNDS: u32 = 32;

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

    async fn send_text_chunk(
        &self,
        session_id: &str,
        text: &str,
    ) -> acp::Result<()> {
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

    async fn execute_llm_streaming(
        &self,
        session_id: &str,
        input: &Input,
        client: &dyn Client,
        abort_signal: &AbortSignal,
    ) -> Result<(String, Vec<ToolCall>), acp::Error> {
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
                                    acp::SessionUpdate::AgentMessageChunk(
                                        acp::ContentChunk::new(chunk.into()),
                                    ),
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

        let (text, tool_calls) = handler.take();
        Ok((text, tool_calls))
    }

    async fn execute_llm_non_streaming(
        &self,
        session_id: &str,
        input: &Input,
        client: &dyn Client,
        abort_signal: &AbortSignal,
    ) -> Result<(String, Vec<ToolCall>), acp::Error> {
        let output = tokio::select! {
            result = client.chat_completions(input.clone()) => {
                result.map_err(|e| acp::Error::new(-32603, e.to_string()))?
            }
            _ = wait_abort_signal(abort_signal) => {
                return Ok((String::new(), vec![]));
            }
        };

        if !output.text.is_empty() {
            self.send_text_chunk(session_id, &output.text).await?;
        }

        Ok((output.text, output.tool_calls))
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

        let agent = self
            .config
            .read()
            .retrieve_agent(&self.agent_name)
            .map_err(|e| acp::Error::new(-32603, format!("Failed to retrieve agent: {e}")))?;

        let mut input = Input::from_str(&self.config, &prompt_text, Some(agent));
        let client = input
            .create_client()
            .map_err(|e| acp::Error::new(-32603, format!("Failed to create client: {e}")))?;

        let mut round = 0u32;
        loop {
            if abort_signal.aborted() {
                return Ok(acp::PromptResponse::new(acp::StopReason::EndTurn));
            }

            let (output, tool_calls) = if input.stream() {
                self.execute_llm_streaming(&session_key, &input, client.as_ref(), &abort_signal)
                    .await?
            } else {
                self.execute_llm_non_streaming(&session_key, &input, client.as_ref(), &abort_signal)
                    .await?
            };

            if tool_calls.is_empty() {
                return Ok(acp::PromptResponse::new(acp::StopReason::EndTurn));
            }

            round += 1;
            if round > MAX_TOOL_CALL_ROUNDS {
                self.send_text_chunk(
                    &session_key,
                    "\n[Error: maximum tool call rounds exceeded]",
                )
                .await?;
                return Ok(acp::PromptResponse::new(acp::StopReason::EndTurn));
            }

            let tool_results = match eval_tool_calls_async(&self.config, tool_calls, &abort_signal).await {
                Ok(results) => results,
                Err(e) => {
                    self.send_text_chunk(
                        &session_key,
                        &format!("\n[Tool error: {e}]"),
                    )
                    .await?;
                    return Ok(acp::PromptResponse::new(acp::StopReason::EndTurn));
                }
            };

            input = input.merge_tool_results(output, tool_results);
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
    use agent_client_protocol::Agent;
    use tokio::task::LocalSet;

    fn run_local<F: std::future::Future>(future: F) -> F::Output {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build ACP server test runtime");
        let local_set = LocalSet::new();
        local_set.block_on(&rt, future)
    }

    fn test_config() -> GlobalConfig {
        use crate::config::Config;
        use parking_lot::RwLock;
        use std::sync::Arc;

        Arc::new(RwLock::new(Config::default()))
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
}
