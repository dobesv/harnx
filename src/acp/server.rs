use agent_client_protocol::{self as acp, Client as _};
use std::{cell::RefCell, collections::HashMap, rc::Rc};
use uuid::Uuid;

const PLACEHOLDER_RESPONSE: &str = "Prompt received (agent execution will be wired in T11)";

pub struct HarnxAgent {
    agent_name: String,
    sessions: RefCell<HashMap<String, HarnxSession>>,
    connection: RefCell<Option<Rc<acp::AgentSideConnection>>>,
}

#[derive(Debug, Clone)]
struct HarnxSession {
    id: String,
    messages: Vec<String>,
    cancelled: bool,
}

impl HarnxAgent {
    pub fn new(agent_name: String) -> Self {
        Self {
            agent_name,
            sessions: RefCell::new(HashMap::new()),
            connection: RefCell::new(None),
        }
    }

    pub fn set_connection(&self, conn: Rc<acp::AgentSideConnection>) {
        self.connection.replace(Some(conn));
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
            messages: Vec::new(),
            cancelled: false,
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
        let incoming_messages: Vec<String> =
            args.prompt.iter().map(content_block_to_text).collect();
        let notification_session_id = {
            let mut sessions = self.sessions.borrow_mut();
            let session = sessions
                .get_mut(session_key.as_str())
                .ok_or_else(acp::Error::invalid_params)?;
            session.cancelled = false;
            session.messages.extend(incoming_messages);
            session.id.clone()
        };

        let connection = self.connection.borrow().clone();
        if let Some(connection) = connection {
            let notification = acp::SessionNotification::new(
                acp::SessionId::new(notification_session_id),
                acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                    PLACEHOLDER_RESPONSE.to_string().into(),
                )),
            );
            connection.session_notification(notification).await?;
        }

        Ok(acp::PromptResponse::new(acp::StopReason::EndTurn))
    }

    async fn cancel(&self, args: acp::CancelNotification) -> acp::Result<()> {
        let session_id = args.session_id.0;
        let mut sessions = self.sessions.borrow_mut();
        let session = sessions
            .get_mut(session_id.as_ref())
            .ok_or_else(acp::Error::invalid_params)?;
        session.cancelled = true;
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

    #[test]
    fn test_new_session_returns_unique_ids() {
        run_local(async {
            let agent = HarnxAgent::new("test".to_string());
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
        run_local(async {
            let agent = HarnxAgent::new("test".to_string());
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
            assert!(session.cancelled);
        });
    }

    #[test]
    fn test_cancel_unknown_session_errors() {
        run_local(async {
            let agent = HarnxAgent::new("test".to_string());

            let result = agent
                .cancel(acp::CancelNotification::new("nonexistent".to_string()))
                .await;

            assert!(result.is_err());
        });
    }
}
