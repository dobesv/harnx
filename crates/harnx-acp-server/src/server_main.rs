//! Public entry point for running the ACP server over stdin/stdout.
//!
//! [`run`] spawns a dedicated thread with a single-threaded tokio runtime
//! so ACP server can own its local state and stdio event loop.

use std::sync::Arc;

use agent_client_protocol as acp;
use agent_client_protocol::schema::{
    AuthenticateRequest, CancelNotification, InitializeRequest, NewSessionRequest, PromptRequest,
};
use anyhow::{anyhow, Context, Result};
use harnx_runtime::config::GlobalConfig;

use crate::HarnxAgent;

/// Run the ACP server on its own thread with a current-thread tokio runtime.
pub async fn run(config: GlobalConfig, agent_name: String) -> Result<()> {
    use tokio::task::LocalSet;

    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("acp-server".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    let _ =
                        result_tx.send(Err(anyhow!("Failed to create ACP server runtime: {err}")));
                    return;
                }
            };

            let local_set = LocalSet::new();
            let result =
                local_set.block_on(&runtime, async move { run_local(config, agent_name).await });
            let _ = result_tx.send(result);
        })
        .context("Failed to start ACP server thread")?;

    match result_rx.await {
        Ok(result) => result,
        Err(_) => Err(anyhow!("ACP server thread panicked")),
    }
}

async fn run_local(config: GlobalConfig, agent_name: String) -> Result<()> {
    let config_for_cleanup = config.clone();
    // `HARNX_MCP_KEEP_SERVICES_AFTER_DISCOVERY` is an INTERNAL, undocumented
    // flag (not a user-facing config knob). The ACP server runs on a
    // current-thread runtime and re-runs MCP tool discovery on every prompt;
    // without this, discovery would invalidate freshly-connected MCP services
    // and churn subprocesses each turn (see #988). Set here at process startup
    // before the async runtime spawns any worker threads, so the `set_var` is
    // effectively single-threaded (the `unsafe` contract is upheld).
    unsafe { std::env::set_var("HARNX_MCP_KEEP_SERVICES_AFTER_DISCOVERY", "1") };
    let agent = Arc::new(HarnxAgent::new(agent_name, config));

    let result = acp::Agent
        .builder()
        .name("harnx-acp-server")
        .on_receive_request_from(
            acp::Client,
            {
                let agent = Arc::clone(&agent);
                async move |request: InitializeRequest, responder, _cx| {
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
                async move |request: AuthenticateRequest, responder, _cx| {
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
                async move |request: NewSessionRequest, responder, cx| {
                    agent.set_connection(cx.clone()).await;
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
                async move |request: PromptRequest, responder, cx| {
                    agent.set_connection(cx.clone()).await;
                    // Offload the long-running prompt to a local task so the
                    // ACP dispatch loop remains free to process concurrent
                    // notifications (e.g. `session/cancel`) while the prompt
                    // is running.  Without this, the cancel notification would
                    // be queued behind the prompt handler and never dispatched
                    // until after the prompt completes, breaking cancel
                    // propagation to sub-agents.
                    let task_agent = Arc::clone(&agent);
                    tokio::task::spawn_local(async move {
                        let result = task_agent.prompt(request).await;
                        match result {
                            Ok(response) => {
                                let _ = responder.respond(response);
                            }
                            Err(err) => {
                                let _ = responder.respond_with_error(err);
                            }
                        }
                    });
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
                    agent.cancel(notification).await
                }
            },
            acp::on_receive_notification!(),
        )
        .connect_to(acp::Stdio::new())
        .await;

    match tokio::task::spawn_blocking(move || config_for_cleanup.write().exit_session()).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => log::warn!("Failed to persist ACP session on exit: {e}"),
        Err(e) => log::warn!("Failed to persist ACP session on exit: {e}"),
    }

    result.map_err(|err| anyhow!("ACP server I/O error: {err}"))?;
    Ok(())
}
