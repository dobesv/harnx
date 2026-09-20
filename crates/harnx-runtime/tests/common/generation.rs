use anyhow::Result;
use harnx_runtime::nats_worker::NatsSessionLogBackend;

/// A worker-side backend for `session`, as the worker builds one for the turn
/// it owns.
///
/// Takes no lease: a backend carries none any more. Every worker-originated
/// append passes the live lease per call, which is what stamps the entry's
/// revision and refuses the write once the lease is gone.
pub async fn fenced_backend(
    js: &async_nats::jetstream::Context,
    session: &str,
) -> Result<NatsSessionLogBackend> {
    Ok(NatsSessionLogBackend::new(js.clone(), session, 1))
}
