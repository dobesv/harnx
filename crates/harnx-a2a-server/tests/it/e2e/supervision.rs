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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_sigkill_fleet_restart_while_parked_later_reply_completes() -> Result<()> {
    use std::os::unix::process::ExitStatusExt;
    let mut server = Server::start_script(Script::Tool).await?;
    let response = server.rpc(RpcCall::new("runner", "alice", "SendMessage", json!({
        "message":{"messageId":"park-before-kill","role":"ROLE_USER","parts":[{"text":"Use the tool"}]}
    }))).await?;
    let task = &response["result"]["task"];
    assert_eq!(
        task["status"]["state"], "TASK_STATE_INPUT_REQUIRED",
        "{response}"
    );
    let id = task["id"].as_str().context("task id")?;
    let local = task["contextId"].as_str().context("context id")?;
    let session = server.h.session(Some(local), &alice()).await?;
    let before = tokio::time::timeout(DEADLINE, async {
        loop {
            let context = server
                .h
                .store
                .read_context(session.storage_key())
                .await?
                .context("context")?;
            if context.document.owner.is_none() {
                return Ok::<_, anyhow::Error>(context);
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await??;
    server._process.child.start_kill()?;
    assert_eq!(
        poll_exit(REAP_DEADLINE, || server._process.child.try_wait())?.signal(),
        Some(9)
    );
    let restarted = Server::start_replica(server.h.clone(), "a2a-restarted.log").await?;
    let read = restarted
        .rpc(RpcCall::new("runner", "alice", "GetTask", json!({"id":id})))
        .await?;
    assert_eq!(
        read["result"]["status"]["state"], "TASK_STATE_INPUT_REQUIRED",
        "{read}"
    );
    let reply = restarted.rpc(RpcCall::new("runner", "alice", "SendMessage", json!({
        "message":{"messageId":"after-restart","role":"ROLE_USER","contextId":local,"parts":[{"text":"y"}]}
    }))).await?;
    assert_eq!(reply["result"]["task"]["id"], id);
    assert_eq!(
        reply["result"]["task"]["status"]["state"], "TASK_STATE_COMPLETED",
        "{reply}"
    );
    let after = server
        .h
        .store
        .read_context(session.storage_key())
        .await?
        .context("context")?;
    let original = before.document.state.active.as_ref().unwrap();
    let settled = after.document.state.active.as_ref().unwrap();
    assert_eq!(original.message, settled.message);
    assert_eq!(
        original.admission.invocation_id,
        settled.admission.invocation_id
    );
    assert_eq!(original.admission.prompt_id, settled.admission.prompt_id);
    assert_eq!(
        original.admission.fixed_predecessor,
        settled.admission.fixed_predecessor
    );
    assert_eq!(server.h.llm.requests.lock().len(), 1);
    Ok(())
}
