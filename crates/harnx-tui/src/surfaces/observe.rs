//! Subscribe first, then refresh durable history; advisories only wake us.
use super::{SurfaceEvent, SurfaceSnapshot};
use crate::types::TuiEvent;
use harnx_runtime::config::GlobalConfig;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

const REFRESH_INTERVAL: Duration = Duration::from_secs(1);

pub(super) fn spawn(
    config: GlobalConfig,
    event_tx: UnboundedSender<TuiEvent>,
    target: super::Target,
    generation: u64,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if let Err(error) = observe_connection(&config, &event_tx, &target, generation).await {
                let _ = event_tx.send(TuiEvent::Surface(SurfaceEvent::Snapshot {
                    generation,
                    surfaces: Err(format!("{error:#}")),
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
    target: &super::Target,
    generation: u64,
) -> anyhow::Result<()> {
    let mut stream = crate::interaction_confirmation::attach_event_stream(config, target).await?;
    let mut last: Option<Vec<SurfaceSnapshot>> = None;
    loop {
        let surfaces = SurfaceSnapshot::from_entries(stream.history())?;
        if last.as_ref() != Some(&surfaces) {
            event_tx.send(TuiEvent::Surface(SurfaceEvent::Snapshot {
                generation,
                surfaces: Ok(surfaces.clone()),
            }))?;
            last = Some(surfaces);
        }
        // Refresh even with no advisory: `ToolResults` and surface-action
        // records are durable control records.
        if let Ok(None) = tokio::time::timeout(REFRESH_INTERVAL, stream.next()).await {
            return Ok(());
        }
        stream.refresh_history().await?;
    }
}
