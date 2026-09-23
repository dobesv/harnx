//! Public entry point for running the ACP server over stdin/stdout.
//!
//! Phase 1: stdio JSON-RPC transport using SDK `Agent::builder().connect_to()`.
//! All logging directed to stderr; stdout carries only protocol frames.

use std::sync::Arc;

use agent_client_protocol as acp;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::HarnxAgent;

/// Run ACP server over stdio.
///
/// This function runs the ACP protocol loop on the caller's tokio runtime.
/// The caller must have an active tokio runtime (e.g., via `#[tokio::main]`).
pub async fn run(agent_name: String) -> anyhow::Result<()> {
    run_stdio(agent_name).await
}

async fn run_stdio(agent_name: String) -> anyhow::Result<()> {
    // Redirect all logs to stderr (tracing-subscriber default).
    // The caller (binary) should initialize logging before calling run().
    let agent = Arc::new(HarnxAgent::new(agent_name));

    // Create byte streams from stdin/stdout for stdio transport.
    // Use Tokio's async stdio and wrap with compat for futures-io traits.
    let stdout = tokio::io::stdout().compat_write();
    let stdin = tokio::io::stdin().compat();
    let byte_streams = acp::ByteStreams::new(stdout, stdin);

    acp::Agent
        .builder()
        .name("harnx-acp-server")
        .on_receive_request_from(
            acp::Client,
            {
                let agent = Arc::clone(&agent);
                async move |request: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    let response = agent.initialize(request).await?;
                    responder.respond(response)
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request_from(
            acp::Client,
            {
                let agent = Arc::clone(&agent);
                async move |request: agent_client_protocol::schema::v1::AuthenticateRequest,
                            responder,
                            _cx| {
                    let response = agent.authenticate(request).await?;
                    responder.respond(response)
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request_from(
            acp::Client,
            {
                let agent = Arc::clone(&agent);
                async move |request: agent_client_protocol::schema::v1::NewSessionRequest,
                            responder,
                            _cx| {
                    let response = agent.new_session(request).await?;
                    responder.respond(response)
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request_from(
            acp::Client,
            {
                let agent = Arc::clone(&agent);
                async move |request: agent_client_protocol::schema::v1::PromptRequest,
                            responder,
                            _cx| match agent.prompt(request).await {
                    Ok(response) => responder.respond(response),
                    Err(error) => responder.respond_with_error(error),
                }
            },
            acp::on_receive_request!(),
        )
        .connect_to(byte_streams)
        .await?;

    Ok(())
}
