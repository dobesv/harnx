//! Public entry point for running the ACP server over stdin/stdout.
//!
//! NATS-backed ACP server with:
//! - Off-loop prompt execution (allows cancel mid-turn)
//! - In-order streaming via single drain task
//! - Connection context for sending notifications

use std::sync::Arc;

use agent_client_protocol as acp;
use agent_client_protocol::schema::v1::{
    CancelNotification, CloseSessionRequest, CloseSessionResponse, ListSessionsRequest,
    LoadSessionRequest, LoadSessionResponse, NewSessionRequest, NewSessionResponse, PromptRequest,
    PromptResponse, ResumeSessionRequest, ResumeSessionResponse,
};
use agent_client_protocol::ConnectTo;
use tokio_util::compat::{Compat, TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::{AcpConnection, HarnxAgent};

macro_rules! add_request_handler {
    ($builder:expr, $handler:expr) => {
        $builder.on_receive_request_from(acp::Client, $handler, acp::on_receive_request!())
    };
}

macro_rules! add_notification_handler {
    ($builder:expr, $handler:expr) => {
        $builder.on_receive_notification_from(
            acp::Client,
            $handler,
            acp::on_receive_notification!(),
        )
    };
}
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
    build_agent(agent).connect_to(streams).await?;
    Ok(())
}

fn build_agent(agent: Arc<HarnxAgent>) -> impl acp::ConnectTo<acp::Client> {
    let (a1, a2, a3, a4) = (
        Arc::clone(&agent),
        Arc::clone(&agent),
        Arc::clone(&agent),
        Arc::clone(&agent),
    );
    let (a5, a6, a7, a8, a9) = (
        Arc::clone(&agent),
        Arc::clone(&agent),
        Arc::clone(&agent),
        Arc::clone(&agent),
        agent,
    );

    let builder = acp::Agent.builder().name("harnx-acp-server");
    let builder = add_request_handler!(builder, async move |r, resp, _| resp
        .respond(a1.initialize(r).await?));
    let builder = add_request_handler!(builder, async move |r, resp, _| resp
        .respond(a2.authenticate(r).await?));
    let builder = add_request_handler!(builder, async move |r, resp, cx| handle_new_session(
        &a3, r, resp, cx
    )
    .await);
    let builder = add_request_handler!(builder, async move |r, resp, cx| handle_load_session(
        &a4, r, resp, cx
    )
    .await);
    let builder = add_request_handler!(builder, async move |r, resp, cx| handle_list_sessions(
        &a5, r, resp, cx
    )
    .await);
    let builder = add_request_handler!(builder, async move |r, resp, cx| handle_resume_session(
        &a6, r, resp, cx
    )
    .await);
    let builder = add_request_handler!(builder, async move |r, resp, cx| handle_close_session(
        &a7, r, resp, cx
    )
    .await);
    let builder = add_request_handler!(builder, async move |r, resp, cx| handle_prompt_request(
        &a8, r, resp, cx
    )
    .await);
    add_notification_handler!(builder, async move |n, cx| {
        handle_cancel_notification(&a9, n, cx).await;
        Ok(())
    })
}

async fn handle_list_sessions(
    agent: &HarnxAgent,
    request: ListSessionsRequest,
    responder: acp::Responder<acp::schema::v1::ListSessionsResponse>,
    _cx: AcpConnection,
) -> acp::Result<()> {
    responder.respond(agent.list_sessions(request).await?)
}

async fn handle_new_session(
    agent: &HarnxAgent,
    request: NewSessionRequest,
    responder: acp::Responder<NewSessionResponse>,
    cx: AcpConnection,
) -> acp::Result<()> {
    agent.set_connection(cx).await;
    responder.respond(agent.new_session(request).await?)
}

async fn handle_load_session(
    agent: &HarnxAgent,
    request: LoadSessionRequest,
    responder: acp::Responder<LoadSessionResponse>,
    cx: AcpConnection,
) -> acp::Result<()> {
    agent.set_connection(cx).await;
    responder.respond(agent.load_session(request).await?)
}

async fn handle_resume_session(
    agent: &HarnxAgent,
    request: ResumeSessionRequest,
    responder: acp::Responder<ResumeSessionResponse>,
    cx: AcpConnection,
) -> acp::Result<()> {
    agent.set_connection(cx).await;
    responder.respond(agent.resume_session(request).await?)
}

async fn handle_close_session(
    agent: &HarnxAgent,
    request: CloseSessionRequest,
    responder: acp::Responder<CloseSessionResponse>,
    cx: AcpConnection,
) -> acp::Result<()> {
    agent.set_connection(cx).await;
    responder.respond(agent.close_session(request).await?)
}

async fn handle_cancel_notification(
    agent: &HarnxAgent,
    notification: CancelNotification,
    cx: AcpConnection,
) {
    agent.set_connection(cx).await;
    if let Err(error) = agent.cancel(notification).await {
        tracing::warn!("cancel failed: {error:#}");
    }
}

async fn handle_prompt_request(
    agent: &Arc<HarnxAgent>,
    request: PromptRequest,
    responder: acp::Responder<PromptResponse>,
    cx: AcpConnection,
) -> acp::Result<()> {
    agent.set_connection(cx).await;
    spawn_prompt_request(Arc::clone(agent), request, responder);
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
