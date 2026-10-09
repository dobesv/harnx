use super::admission::{remove_owner_lease, reservation};
use super::*;
use a2a_lf::TaskState;

fn supervise(t: &TwoBackends, replica: &Replica) {
    replica
        .backend
        .runner
        .start_supervision(harnx_a2a_server::runner::SupervisionConfig {
            exports: vec![t.h.export.clone()],
            config: t.h.config.clone(),
            route: SessionActivationRoute::ClusterShared,
            abort: replica.backend.abort.clone(),
        });
}
async fn session_for(replica: &Replica, h: &Harness, local: &str) -> Result<NatsSession> {
    replica
        .backend
        .runner
        .session(SessionRequest {
            export: &h.export,
            owner: &alice().into(),
            local_id: Some(local),
            global_config: &h.config,
            activation_route: SessionActivationRoute::ClusterShared,
            abort: replica.backend.abort.clone(),
        })
        .await
}
async fn terminal(t: &TwoBackends, id: &str) -> Result<harnx_a2a_server::store::TaskRecord> {
    tokio::time::timeout(DEADLINE, async {
        loop {
            if let Some(task) =
                t.b.backend
                    .store
                    .get_task_for_export(&t.h.export, &alice().into(), id)
                    .await?
            {
                if task.task.status.state.is_terminal() {
                    return Ok(task);
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .context("background terminal deadline")?
}
async fn requested(t: &TwoBackends, count: usize) -> Result<()> {
    tokio::time::timeout(DEADLINE, async {
        while t.h.llm.requests.lock().len() < count {
            t.h.llm.requested.notified().await;
        }
    })
    .await?;
    Ok(())
}

#[cfg(unix)]
mod broker;
mod cancel;
mod recovery;
