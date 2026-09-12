//! Recovery boundaries shared by all NATS roles. Retry transport work only
//! when replay is safe; never retry a handler with unknown side effects.

use anyhow::{Context, Result};
use async_nats::jetstream::{self, kv};
use futures_util::{stream, Stream, StreamExt};
use std::{fmt::Display, future::Future, pin::Pin, time::Duration};
use tokio::time::{sleep, timeout_at, Instant};

pub const RECOVERY_TIMEOUT: Duration = Duration::from_secs(15);
const RETRY_DELAY: Duration = Duration::from_millis(100);

/// Retry a read without interpreting a failed read as absence. Parsing and
/// application validation belong outside this closure and are never retried.
pub async fn read<T, E, F, Fut>(operation: F) -> Result<T>
where
    E: Into<anyhow::Error> + Display,
    F: FnMut() -> Fut,
    Fut: Future<Output = std::result::Result<T, E>>,
{
    retry_until(Instant::now() + RECOVERY_TIMEOUT, operation, |_| true).await
}

/// The deadline is fixed across retries. Callers of writes must supply stable
/// deduplication/CAS identity and reject semantic conflicts in `retryable`.
pub async fn retry_until<T, E, F, Fut>(
    deadline: Instant,
    mut operation: F,
    retryable: impl Fn(&E) -> bool,
) -> Result<T>
where
    E: Into<anyhow::Error> + Display,
    F: FnMut() -> Fut,
    Fut: Future<Output = std::result::Result<T, E>>,
{
    loop {
        anyhow::ensure!(
            Instant::now() < deadline,
            "NATS recovery deadline exceeded; completion unconfirmed"
        );
        let result = timeout_at(deadline, operation())
            .await
            .context("NATS recovery deadline exceeded; completion unconfirmed")?;
        match result {
            Ok(value) => return Ok(value),
            Err(error) if retryable(&error) && Instant::now() + RETRY_DELAY < deadline => {
                log::debug!("retrying NATS transport operation: {error}");
                sleep(RETRY_DELAY).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

pub fn transient_publish(error: &jetstream::context::PublishError) -> bool {
    matches!(
        error.kind(),
        jetstream::context::PublishErrorKind::TimedOut
            | jetstream::context::PublishErrorKind::BrokenPipe
    )
}

pub type KvUpdates = Pin<Box<dyn Stream<Item = Result<()>> + Send>>;

/// KV watches are wakeups, never the source of truth. Reopen failed watches
/// and periodically wake readers even if a consumer silently stops delivering.
/// Each wakeup must be followed by a current authoritative state read.
pub async fn kv_updates(store: kv::Store) -> Result<KvUpdates> {
    let watch = read(|| store.watch_all()).await?;
    Ok(Box::pin(stream::unfold(
        (store, watch),
        |(store, mut watch)| async move {
            let result = match tokio::time::timeout(Duration::from_secs(1), watch.next()).await {
                Ok(Some(Ok(_))) | Err(_) => Ok(()),
                _ => match read(|| store.watch_all()).await {
                    Ok(reopened) => {
                        watch = reopened;
                        Ok(())
                    }
                    Err(error) => Err(error),
                },
            };
            Some((result, (store, watch)))
        },
    )))
}
