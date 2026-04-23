//! Public entry point for running the ACP server over stdin/stdout.
//!
//! [`run`] spawns a dedicated thread with a single-threaded tokio runtime
//! (required because ACP uses `!Send` types behind `Rc`), wires `HarnxAgent`
//! to `agent_client_protocol::AgentSideConnection`, and drives the I/O loop
//! until stdin closes.

use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context as TaskContext, Poll};

use agent_client_protocol as acp;
use anyhow::{anyhow, Context, Result};
use harnx_runtime::config::GlobalConfig;
use tokio::io::{AsyncRead as TokioAsyncRead, AsyncWrite as TokioAsyncWrite, ReadBuf};

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

/// Adapter between tokio's AsyncRead/AsyncWrite and futures_io's AsyncRead/AsyncWrite.
/// `agent-client-protocol` takes futures_io traits; stdin/stdout give us tokio traits.
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

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
