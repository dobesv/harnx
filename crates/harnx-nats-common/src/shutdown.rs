//! Graceful-shutdown signal handling shared by the standalone tool/hook
//! server binaries.
//!
//! These binaries run as independently deployed pods with no parent
//! supervisor to clean up after them, so they need to notice a Kubernetes
//! SIGTERM themselves and get a chance to deregister before the pod is
//! killed.

use tokio_util::sync::CancellationToken;

/// Resolves on SIGTERM or Ctrl+C. Mirrors
/// `harnx-proxy-auth`'s `shutdown_signal`, kept in one place now that more
/// than one binary needs it.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        let _ = sigterm.recv().await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

/// Spawn a task that cancels the returned token on SIGTERM/Ctrl+C, for
/// threading into a `serve_with_shutdown`-style entry point.
pub fn cancel_token_on_shutdown_signal() -> CancellationToken {
    let token = CancellationToken::new();
    let signalled = token.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        signalled.cancel();
    });
    token
}
