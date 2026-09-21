use tokio_util::sync::CancellationToken;

/// Policy for registration cleanup on server shutdown.
///
/// In HA deployments with multiple replicas sharing one registration key,
/// use `Expire` to skip delete-on-shutdown and let the KV bucket TTL reap
/// the key after the last replica exits. Singleton servers use the default
/// `DeleteIfCurrent` to remove their registration promptly on shutdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RegistrationShutdown {
    /// Delete the registration key on shutdown if our revision is still current.
    ///
    /// This is the default for singleton tool servers. On a rolling deploy,
    /// a replacement instance may have already overwritten the key; in that
    /// case, the delete is a no-op (wrong revision).
    #[default]
    DeleteIfCurrent,

    /// Skip registration deletion on shutdown; let the bucket TTL expire it.
    ///
    /// Used by HA gateway replicas that share a registration key. Surviving
    /// replicas continue refreshing; the key is removed only after the last
    /// replica exits and the TTL elapses.
    Expire,
}

/// Shutdown and readiness controls for a running toolset server.
pub struct ServeLifecycle {
    shutdown: CancellationToken,
    readiness: Option<harnx_healthz::Readiness>,
    registration_shutdown: RegistrationShutdown,
}

impl ServeLifecycle {
    /// Combine shutdown and readiness handles for `serve_with_shutdown`.
    ///
    /// Uses the default `RegistrationShutdown::DeleteIfCurrent` policy.
    pub fn new(shutdown: CancellationToken, readiness: Option<harnx_healthz::Readiness>) -> Self {
        Self {
            shutdown,
            readiness,
            registration_shutdown: RegistrationShutdown::default(),
        }
    }

    /// Set the registration shutdown policy.
    ///
    /// Use `RegistrationShutdown::Expire` for HA gateway replicas that share
    /// a registration key across multiple instances.
    pub fn with_registration_shutdown(mut self, policy: RegistrationShutdown) -> Self {
        self.registration_shutdown = policy;
        self
    }

    pub(super) fn into_parts(
        self,
    ) -> (
        CancellationToken,
        Option<harnx_healthz::Readiness>,
        RegistrationShutdown,
    ) {
        (self.shutdown, self.readiness, self.registration_shutdown)
    }
}
