use super::admission::{remove_owner_lease, reservation};
use super::*;
use a2a_lf::{StreamResponse, TaskState};
use harnx_runtime::{a2a_events, nats_admin};

fn part_text(part: &a2a_lf::Part) -> &str {
    match &part.content {
        a2a_lf::PartContent::Text(text) => text,
        _ => "",
    }
}

async fn settled(t: &TwoBackends) -> Result<harnx_a2a_server::store::FirstMessageReservation> {
    let first = reservation(t).await?;
    tokio::time::timeout(DEADLINE, async {
        loop {
            let context =
                t.h.store
                    .read_context(&first.allocation.storage_key)
                    .await?
                    .context("context")?;
            let active = context.document.state.active.as_ref().context("active")?;
            if active.stop_confirmed
                && active.projections.archive
                && active.projections.message_mapping
                && active.projections.final_event
                && active.publication.pending.is_none()
                && context.document.owner.is_none()
            {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await??;
    Ok(first)
}

mod capacity;
mod gc;
mod payload;
mod permissions;
