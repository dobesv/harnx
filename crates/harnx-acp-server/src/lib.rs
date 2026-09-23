//! harnx-acp-server — ACP server front-end over stdio.
//!
//! Implements ACP v1 protocol handshake (initialize, session/new) without NATS.
//! Phase 1 scaffold for issue #1346.

use std::sync::Arc;

use agent_client_protocol as acp;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, AuthenticateRequest, AuthenticateResponse, Implementation,
    InitializeRequest, InitializeResponse, NewSessionRequest, NewSessionResponse, PromptRequest,
    PromptResponse, SessionId,
};

pub mod server_main;

pub use server_main::run;

/// In-memory session state for Phase 1 (no NATS binding yet).
struct SessionContext {
    #[allow(dead_code)]
    session_id: String,
}

/// The ACP agent implementation for harnx.
pub struct HarnxAgent {
    agent_name: String,
    sessions: Arc<tokio::sync::RwLock<Vec<Arc<SessionContext>>>>,
}

impl HarnxAgent {
    pub fn new(agent_name: String) -> Self {
        Self {
            agent_name,
            sessions: Arc::new(tokio::sync::RwLock::new(Vec::new())),
        }
    }

    /// Handle `initialize` request — negotiate protocol v1, advertise minimal caps.
    pub async fn initialize(&self, _request: InitializeRequest) -> acp::Result<InitializeResponse> {
        // We only support v1 (default from SDK "2.2.0", NOT unstable_protocol_v2).
        // Always advertise v1 instead of echoing an unsupported client version.
        Ok(
            InitializeResponse::new(agent_client_protocol::schema::ProtocolVersion::V1)
                .agent_capabilities(AgentCapabilities::new())
                .agent_info(
                    Implementation::new("harnx".to_string(), env!("CARGO_PKG_VERSION").to_string())
                        .title(self.agent_name.clone()),
                ),
        )
    }

    /// Handle `authenticate` request — no-op for Phase 1.
    pub async fn authenticate(
        &self,
        _request: AuthenticateRequest,
    ) -> acp::Result<AuthenticateResponse> {
        Ok(AuthenticateResponse::default())
    }

    /// Handle `session/new` — generate in-memory session ID (no NATS yet).
    pub async fn new_session(
        &self,
        _request: NewSessionRequest,
    ) -> acp::Result<NewSessionResponse> {
        let session_id = uuid::Uuid::new_v4().to_string();
        let ctx = Arc::new(SessionContext {
            session_id: session_id.clone(),
        });
        self.sessions.write().await.push(ctx);
        Ok(NewSessionResponse::new(SessionId::new(session_id)))
    }

    /// Handle `session/prompt` — stub for Phase 1 (will be implemented in Phase 2).
    pub async fn prompt(&self, _request: PromptRequest) -> acp::Result<PromptResponse> {
        // Phase 1: stub that returns empty response. Phase 2 will bind to NATS.
        // Return an error indicating not implemented yet.
        Err(acp::Error::new(
            -32603,
            "session/prompt not implemented in Phase 1",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_agent() -> HarnxAgent {
        HarnxAgent::new("test-agent".to_string())
    }

    #[tokio::test]
    async fn initialize_returns_protocol_version_1_for_unsupported_requests() {
        let agent = test_agent();

        for requested_version in [2_u16, 99] {
            let request = InitializeRequest::new(
                agent_client_protocol::schema::ProtocolVersion::from(requested_version),
            );
            let response = agent.initialize(request).await.unwrap();

            assert_eq!(
                response.protocol_version,
                agent_client_protocol::schema::ProtocolVersion::V1
            );
        }
    }

    #[tokio::test]
    async fn initialize_advertises_minimal_capabilities() {
        let agent = test_agent();
        let request = InitializeRequest::new(agent_client_protocol::schema::ProtocolVersion::V1);

        let response = agent.initialize(request).await.unwrap();

        // Agent capabilities should be present (empty/minimal for Phase 1)
        // No need to assert specific capability fields - the builder chain sets defaults
        assert!(response.agent_info.is_some());
        let agent_info = response.agent_info.unwrap();
        assert_eq!(agent_info.name, "harnx");
        assert!(agent_info.title.is_some());
        assert_eq!(agent_info.title.unwrap(), "test-agent");
    }

    #[tokio::test]
    async fn session_new_returns_session_id() {
        let agent = test_agent();
        // NewSessionRequest::new requires a cwd argument
        let request = NewSessionRequest::new(std::env::current_dir().unwrap());

        let response = agent.new_session(request).await.unwrap();

        // Session ID should be a valid UUID string
        assert!(!response.session_id.0.is_empty());
        assert!(uuid::Uuid::parse_str(&response.session_id.0).is_ok());
    }

    #[tokio::test]
    async fn authenticate_responds_ok() {
        let agent = test_agent();
        // AuthenticateRequest::new requires a method_id - use agent auth method
        let method_id = agent_client_protocol::schema::v1::AuthMethodId::new("agent".to_string());
        let request = AuthenticateRequest::new(method_id);

        let response = agent.authenticate(request).await.unwrap();
        // Default response is fine - just checking it doesn't error
        let _ = response;
    }

    #[tokio::test]
    async fn stub_prompt_returns_error() {
        let agent = test_agent();
        let request = PromptRequest::new(SessionId::new("test".to_string()), vec![]);

        let result = agent.prompt(request).await;
        assert!(result.is_err());
    }
}
