use tokio_util::sync::CancellationToken;

/// Shutdown and readiness controls for a running hookset server.
pub struct ServeLifecycle {
    shutdown: CancellationToken,
    readiness: Option<harnx_healthz::Readiness>,
}

impl ServeLifecycle {
    /// Combine shutdown and readiness handles for `serve_with_shutdown`.
    pub fn new(shutdown: CancellationToken, readiness: Option<harnx_healthz::Readiness>) -> Self {
        Self {
            shutdown,
            readiness,
        }
    }

    pub(super) fn into_parts(self) -> (CancellationToken, Option<harnx_healthz::Readiness>) {
        (self.shutdown, self.readiness)
    }
}
