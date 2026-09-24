//! Public entry point for running the ACP server over stdin/stdout.
//!
//! Phase 2: Full NATS binding with:
//! - Off-loop prompt execution (allows cancel mid-turn)
//! - In-order streaming via single drain task
//! - Connection context for sending notifications

use std::sync::Arc;

use agent_client_protocol as acp;
use agent_client_protocol::schema::v1::{
    AuthenticateRequest, CancelNotification, InitializeRequest, NewSessionRequest, PromptRequest,
    PromptResponse,
};
use tokio_util::compat::{Compat, TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::HarnxAgent;

type StdioStreams = acp::ByteStreams<Compat<tokio::io::Stdout>, Compat<tokio::io::Stdin>>;

/// Run ACP server over stdio.
///
/// This function runs the ACP protocol loop on the caller's tokio runtime.
/// The caller must have an active tokio runtime (e.g., via `#[tokio::main]`).
pub async fn run(agent_name: String) -> anyhow::Result<()> {
    run_stdio(agent_name).await
}

async fn run_stdio(agent_name: String) -> anyhow::Result<()> {
    let agent = Arc::new(HarnxAgent::new(agent_name));
    let streams = acp::ByteStreams::new(
        tokio::io::stdout().compat_write(),
        tokio::io::stdin().compat(),
    );
    register_handlers(agent, streams).await
}

async fn register_handlers(agent: Arc<HarnxAgent>, streams: StdioStreams) -> anyhow::Result<()> {
    acp::Agent
        .builder()
        .name("harnx-acp-server")
        .on_receive_request_from(
            acp::Client,
            {
                let agent = Arc::clone(&agent);
                async move |request: InitializeRequest, responder, _cx| {
                    responder.respond(agent.initialize(request).await?)
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request_from(
            acp::Client,
            {
                let agent = Arc::clone(&agent);
                async move |request: AuthenticateRequest, responder, _cx| {
                    responder.respond(agent.authenticate(request).await?)
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request_from(
            acp::Client,
            {
                let agent = Arc::clone(&agent);
                async move |request: NewSessionRequest, responder, cx| {
                    agent.set_connection(cx.clone()).await;
                    responder.respond(agent.new_session(request).await?)
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request_from(
            acp::Client,
            {
                let agent = Arc::clone(&agent);
                async move |request: PromptRequest, responder, cx| {
                    agent.set_connection(cx.clone()).await;
                    spawn_prompt_request(Arc::clone(&agent), request, responder);
                    Ok(())
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_notification_from(
            acp::Client,
            {
                let agent = Arc::clone(&agent);
                async move |notification: CancelNotification, cx| {
                    agent.set_connection(cx.clone()).await;
                    if let Err(error) = agent.cancel(notification).await {
                        tracing::warn!("cancel failed: {:#}", error);
                    }
                    Ok(())
                }
            },
            acp::on_receive_notification!(),
        )
        .connect_to(streams)
        .await?;
    Ok(())
}

fn spawn_prompt_request(
    agent: Arc<HarnxAgent>,
    request: PromptRequest,
    responder: acp::Responder<PromptResponse>,
) {
    // Keep the dispatch loop free to process session/cancel while a turn runs.
    tokio::spawn(async move {
        match agent.prompt(request).await {
            Ok(response) => {
                let _ = responder.respond(response);
            }
            Err(error) => {
                let _ = responder.respond_with_error(error);
            }
        }
    });
}
