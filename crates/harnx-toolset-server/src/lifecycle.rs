use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::REGISTRATION_REFRESH_INTERVAL;

/// Policy for registration cleanup on server shutdown.
///
/// The default is `Expire`, which is safe for both singleton and HA deployments.
/// `DeleteIfCurrent` is preserved for backward compatibility but is unsafe for
/// shared registration keys and should only be used by singletons that never
/// share their identity across multiple replicas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RegistrationShutdown {
    /// Skip registration deletion on shutdown; let the bucket TTL expire it.
    ///
    /// This is the default. NATS KV registrations have a 90-second TTL and
    /// servers refresh every 30 seconds. When a replica shuts down, surviving
    /// replicas continue refreshing. The key is removed only after all replicas
    /// exit and the TTL (90 seconds from the last refresh) elapses.
    ///
    /// This policy works for both singleton servers and HA deployments where
    /// multiple replicas share the same registration key.
    #[default]
    Expire,

    /// Delete the registration key on shutdown if our revision is still current.
    ///
    /// **Warning**: This policy is unsafe for shared registration keys. If multiple
    /// replicas share the same key (via identical instance ID and identity token),
    /// a shutting-down replica that holds the latest revision will delete the key
    /// that surviving replicas still need. Survivors cannot be discovered until they
    /// refresh and republish, which may take up to 30 seconds.
    ///
    /// Only use this policy for singleton servers that are guaranteed never to share
    /// their registration identity. The revision CAS check only prevents deletion of
    /// a newer replacement's key; it cannot prove exclusive ownership of a shared key.
    DeleteIfCurrent,
}

/// Shutdown and readiness controls for a running toolset server.
pub struct ServeLifecycle {
    shutdown: CancellationToken,
    readiness: Option<harnx_healthz::Readiness>,
    registration_shutdown: RegistrationShutdown,
    refresh_interval: Duration,
}

impl ServeLifecycle {
    /// Combine shutdown and readiness handles for `serve_with_shutdown`.
    ///
    /// Uses the default `RegistrationShutdown::Expire` policy, which is
    /// safe for both singleton and HA deployments.
    pub fn new(shutdown: CancellationToken, readiness: Option<harnx_healthz::Readiness>) -> Self {
        Self {
            shutdown,
            readiness,
            registration_shutdown: RegistrationShutdown::default(),
            refresh_interval: REGISTRATION_REFRESH_INTERVAL,
        }
    }

    /// Set the registration shutdown policy.
    ///
    /// The default (`Expire`) is safe for HA deployments with shared keys.
    /// Use `DeleteIfCurrent` only for singleton servers that never share
    /// their registration identity.
    pub fn with_registration_shutdown(mut self, policy: RegistrationShutdown) -> Self {
        self.registration_shutdown = policy;
        self
    }

    /// Set how often the KV registration is renewed, which defaults to a
    /// third of its TTL. Keep it well under
    /// `harnx_nats_common::registry::REGISTRATION_TTL`, or the registration
    /// expires between renewals. Tests shorten it to observe a renewal.
    pub fn with_refresh_interval(mut self, interval: Duration) -> Self {
        self.refresh_interval = interval;
        self
    }

    pub(super) fn refresh_interval(&self) -> Duration {
        self.refresh_interval
    }

    pub(super) fn readiness(&self) -> Option<&harnx_healthz::Readiness> {
        self.readiness.as_ref()
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
