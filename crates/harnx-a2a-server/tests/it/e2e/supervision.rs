use super::*;
use crate::support::alice;
use harnx_a2a_server::{runner::Runner, store::A2aStore};
use std::sync::Arc;

async fn background_terminal(h: &Harness, id: &str) -> Result<harnx_a2a_server::store::TaskRecord> {
    tokio::time::timeout(DEADLINE, async {
        loop {
            let record = h.task(id).await?;
            if record.task.status.state.is_terminal() {
                return Ok(record);
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .context("traffic-independent terminal deadline")?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn killed_owner_settles_on_real_lease_expiry_while_worker_renews_without_http() -> Result<()>
{
    use std::os::unix::process::ExitStatusExt;
    let mut server = Server::start().await?;
    let response = server.rpc(RpcCall::new("runner", "alice", "SendMessage", json!({
        "message":{"messageId":"sigkill-owner","role":"ROLE_USER","parts":[{"text":"Hold this turn"}]},
        "configuration":{"returnImmediately":true}}))).await?;
    let id = response["result"]["task"]["id"]
        .as_str()
        .context("admitted task")?;
    let context = response["result"]["task"]["contextId"]
        .as_str()
        .context("context")?;
    server.requested(1).await?;
    let storage = harnx_core::session_identity::session_key(Some(&server.h.export.agent), context);
    let leases = server.h.jetstream.get_key_value("harnx_leases").await?;
    let worker_key = format!("sessions/{storage}/lock");
    let worker_before = harnx_nats_common::leader_reads::entry(&leases, &worker_key)
        .await?
        .context("running worker lease")?;
    // Kill only A2A, keeping the independently started worker alive.
    server._process.child.start_kill()?;
    let exit = poll_exit(REAP_DEADLINE, || server._process.child.try_wait())?;
    assert_eq!(exit.signal(), Some(9), "owner must die by SIGKILL");
    let store = Arc::new(A2aStore::new(server.h.metadata.clone()));
    let runner = Runner::new(store);
    let abort = harnx_core::abort::create_abort_signal();
    runner.start_supervision(harnx_a2a_server::runner::SupervisionConfig {
        exports: vec![server.h.export.clone()],
        config: server.h.config.clone(),
        route: harnx_runtime::SessionActivationRoute::ClusterShared,
        abort: abort.clone(),
    });
    tokio::time::timeout(DEADLINE, async {
        loop {
            if let Some(entry) =
                harnx_nats_common::leader_reads::entry(&leases, &worker_key).await?
            {
                if entry.revision > worker_before.revision {
                    return Ok::<_, anyhow::Error>(());
                }
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .context("worker must keep renewing after owner death")??;
    let terminal = background_terminal(&server.h, id).await?;
    assert_eq!(terminal.task.status.state, a2a_lf::TaskState::Failed);
    assert_eq!(server.h.llm.requests.lock().len(), 1);
    let read = runner.live_record(&server.h.export, terminal.clone()).await;
    assert_eq!(read.revision, terminal.revision);
    assert_eq!(
        server
            .h
            .store
            .get_task_for_export(&server.h.export, &alice().into(), id)
            .await?
            .context("retained task")?
            .task
            .status
            .state,
        a2a_lf::TaskState::Failed
    );
    abort.set_ctrlc();
    Ok(())
}
