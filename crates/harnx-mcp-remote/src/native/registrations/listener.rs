//! Debounced upstream catalog notification loop.

use super::Lifecycle;
use crate::native::pool::ListChanged;
use harnx_credentials::Principal;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

/// The background loop that turns pool `list_changed` signals into catalog
/// and registration updates, debounced per (principal, connection).
pub async fn run_list_changed_loop(
    lifecycle: Arc<Lifecycle>,
    mut receiver: tokio::sync::mpsc::Receiver<ListChanged>,
    debounce: Duration,
) {
    let mut pending: HashMap<(Principal, String), Instant> = HashMap::new();
    loop {
        // Sleep until the nearest pending deadline; a new signal just re-arms
        // that key's deadline without resetting any other principal's.
        let next = pending.values().min().copied();
        let due: Vec<(Principal, String)> = tokio::select! {
            signal = receiver.recv() => {
                let Some(signal) = signal else { return };
                pending
                    .entry((signal.principal, signal.connection_id))
                    .and_modify(|deadline| *deadline = Instant::now() + debounce)
                    .or_insert(Instant::now() + debounce);
                continue;
            }
            _ = tokio::time::sleep(next.map_or(Duration::from_secs(3600), |deadline| {
                deadline.saturating_duration_since(Instant::now())
            })) => {
                let now = Instant::now();
                let due: Vec<(Principal, String)> = pending
                    .iter()
                    .filter(|(_, deadline)| **deadline <= now)
                    .map(|(key, _)| key.clone())
                    .collect();
                for key in &due {
                    pending.remove(key);
                }
                due
            }
        };
        for (principal, connection_id) in due {
            lifecycle.apply_list_changed(principal, connection_id).await;
        }
    }
}
