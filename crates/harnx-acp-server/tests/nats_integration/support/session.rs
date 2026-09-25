//! ACP initialization and session creation fixture.

use agent_client_protocol as acp;
use agent_client_protocol::schema::v1::{InitializeRequest, NewSessionRequest, SessionId};
use anyhow::Result;

pub(crate) async fn initialize_and_create_session(
    agent: &harnx_acp_server::HarnxAgent,
) -> Result<SessionId> {
    let initialized = agent
        .initialize(InitializeRequest::new(acp::schema::ProtocolVersion::V1))
        .await?;
    assert_eq!(
        initialized.protocol_version,
        acp::schema::ProtocolVersion::V1
    );

    let session = agent
        .new_session(NewSessionRequest::new(std::env::current_dir()?))
        .await?;
    Ok(session.session_id)
}
