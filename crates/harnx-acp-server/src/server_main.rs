//! Public entry point for running the ACP server over stdin/stdout.
//!
//! [`run`] spawns a dedicated thread with a single-threaded tokio runtime
//! (required because ACP uses `!Send` types behind `Rc`), wires `HarnxAgent`
//! to `agent_client_protocol::AgentSideConnection`, and drives the I/O loop
//! until stdin closes.

use std::rc::Rc;

use agent_client_protocol as acp;
use anyhow::{anyhow, Context, Result};
use harnx_acp::compat::TokioCompat;
use harnx_runtime::config::GlobalConfig;

use crate::HarnxAgent;

/// Run the ACP server on its own thread with a current-thread tokio runtime.
/// ACP uses `!Send` types (`Rc<AgentSideConnection>`) so the multi-threaded
/// runtime that drives the rest of harnx can't host it directly.
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
    let agent = Rc::new(HarnxAgent::new(agent_name, config));
    let agent_for_conn = Rc::clone(&agent);
    let stdin = tokio::io::stdin();
    #[cfg(unix)]
    let stdout = {
        use std::os::fd::AsFd;

        let owned_fd = std::io::stdout()
            .as_fd()
            .try_clone_to_owned()
            .context("Failed to duplicate stdout fd for ACP server")?;
        tokio::fs::File::from_std(std::fs::File::from(owned_fd))
    };
    #[cfg(not(unix))]
    let stdout = tokio::io::stdout();

    let (conn, io_task) = acp::AgentSideConnection::new(
        agent_for_conn,
        TokioCompat::new(stdout),
        TokioCompat::new(stdin),
        |future| {
            tokio::task::spawn_local(future);
        },
    );

    agent.set_connection(Rc::new(conn));
    let result = io_task.await;

    // Persist any remaining session state on shutdown (#232).
    // `exit_session` performs blocking file I/O, so run it on the blocking
    // pool rather than stalling the async runtime thread.
    match tokio::task::spawn_blocking(move || config_for_cleanup.write().exit_session()).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => log::warn!("Failed to persist ACP session on exit: {e}"),
        Err(e) => log::warn!("Failed to persist ACP session on exit: {e}"),
    }

    result.map_err(|err| anyhow!("ACP server I/O error: {err}"))?;
    Ok(())
}
