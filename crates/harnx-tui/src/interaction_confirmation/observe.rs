//! Subscribe first, then refresh durable history. Advisories only wake this reader.
use super::{pending_confirmation, InteractionEvent, Target};
use crate::types::TuiEvent;
use harnx_runtime::config::GlobalConfig;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

const REFRESH_INTERVAL: Duration = Duration::from_secs(1);

pub(super) fn spawn(
    config: GlobalConfig,
    event_tx: UnboundedSender<TuiEvent>,
    target: Target,
    generation: u64,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if let Err(error) = observe_connection(&config, &event_tx, &target, generation).await {
                let _ = event_tx.send(TuiEvent::Interaction(InteractionEvent::Snapshot {
                    generation,
                    pending: Err(format!("{error:#}")),
                }));
            }
            if event_tx.is_closed() {
                return;
            }
            tokio::time::sleep(REFRESH_INTERVAL).await;
        }
    })
}

async fn observe_connection(
    config: &GlobalConfig,
    event_tx: &UnboundedSender<TuiEvent>,
    target: &Target,
    generation: u64,
) -> anyhow::Result<()> {
    let mut stream = super::attach_event_stream(config, target).await?;
    let mut last = None;
    loop {
        let pending = pending_confirmation(stream.history())?;
        if last.as_ref() != Some(&pending) {
            event_tx.send(TuiEvent::Interaction(InteractionEvent::Snapshot {
                generation,
                pending: Ok(pending.clone()),
            }))?;
            last = Some(pending);
        }
        // Refresh even with no advisory: SuspensionReady and responses are durable control records.
        if let Ok(None) = tokio::time::timeout(REFRESH_INTERVAL, stream.next()).await {
            return Ok(());
        }
        stream.refresh_history().await?;
    }
}
