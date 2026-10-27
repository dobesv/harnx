mod fixtures;
use fixtures::*;

use super::*;
use crate::support::alice;
use harnx_a2a_server::store::context::ContextSnapshot;
use harnx_a2a_server::{runner::Runner, store::A2aStore};
use harnx_runtime::NatsSession;
use std::os::unix::process::ExitStatusExt;
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

struct KilledServerContext {
    id: String,
    leases: async_nats::jetstream::kv::Store,
    worker_key: String,
    worker_before_revision: u64,
}

fn kill_and_reap_server(server: &mut Server) -> Result<std::process::ExitStatus> {
    server._process.child.start_kill()?;
    poll_exit(REAP_DEADLINE, || server._process.child.try_wait())
}

struct WorkerLeaseInfo {
    leases: async_nats::jetstream::kv::Store,
    worker_key: String,
    worker_before_revision: u64,
}

async fn query_worker_lease(server: &Server, context: &str) -> Result<WorkerLeaseInfo> {
    let storage = harnx_core::session_identity::session_key(Some(&server.h.export.agent), context);
    let leases = server.h.jetstream.get_key_value("harnx_leases").await?;
    let worker_key = format!("sessions/{storage}/lock");
    let worker_before = harnx_nats_common::leader_reads::entry(&leases, &worker_key)
        .await?
        .context("running worker lease")?;
    Ok(WorkerLeaseInfo {
        leases,
        worker_key,
        worker_before_revision: worker_before.revision,
    })
}

async fn start_and_kill_owner(server: &mut Server) -> Result<KilledServerContext> {
    let response = server
        .rpc(RpcCall::new(
            "runner",
            "alice",
            "SendMessage",
            json!({
                "message": {"messageId": "sigkill-owner", "role": "ROLE_USER", "parts": [{"text": "Hold this turn"}]},
                "configuration": {"returnImmediately": true}
            }),
        ))
        .await?;
    let id = response["result"]["task"]["id"]
        .as_str()
        .context("admitted task")?
        .to_owned();
    let context = response["result"]["task"]["contextId"]
        .as_str()
        .context("context")?;
    server.requested(1).await?;
    let lease_info = query_worker_lease(server, context).await?;
    // Kill only A2A, keeping the independently started worker alive.
    assert_eq!(
        kill_and_reap_server(server)?.signal(),
        Some(9),
        "owner must die by SIGKILL"
    );
    Ok(KilledServerContext {
        id,
        leases: lease_info.leases,
        worker_key: lease_info.worker_key,
        worker_before_revision: lease_info.worker_before_revision,
    })
}

fn spawn_supervision(h: &Harness, runner: &Arc<Runner>, abort: &harnx_core::abort::AbortSignal) {
    runner.start_supervision(harnx_a2a_server::runner::SupervisionConfig {
        exports: vec![h.export.clone()],
        config: h.config.clone(),
        route: harnx_runtime::SessionActivationRoute::ClusterShared,
        abort: abort.clone(),
    });
}

async fn assert_supervised_terminal(h: &Harness, runner: &Runner, id: &str) -> Result<()> {
    let terminal = background_terminal(h, id).await?;
    assert_eq!(terminal.task.status.state, a2a_lf::TaskState::Failed);
    assert_eq!(h.llm.requests.lock().len(), 1);
    let read = runner.live_record(&h.export, terminal.clone()).await;
    assert_eq!(read.revision, terminal.revision);
    assert_eq!(
        h.store
            .get_task_for_export(&h.export, &alice().into(), id)
            .await?
            .context("retained task")?
            .task
            .status
            .state,
        a2a_lf::TaskState::Failed
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn killed_owner_settles_on_real_lease_expiry_while_worker_renews_without_http() -> Result<()>
{
    let mut server = Server::start().await?;
    let ctx = start_and_kill_owner(&mut server).await?;
    let store = Arc::new(A2aStore::new(server.h.metadata.clone()));
    let runner = Runner::new(store);
    let abort = harnx_core::abort::create_abort_signal();
    spawn_supervision(&server.h, &runner, &abort);
    await_worker_lease_renewed(&ctx.leases, &ctx.worker_key, ctx.worker_before_revision).await?;
    assert_supervised_terminal(&server.h, &runner, &ctx.id).await?;
    abort.set_ctrlc();
    Ok(())
}

struct ParkedServerState {
    id: String,
    local: String,
    session: NatsSession,
    before: ContextSnapshot,
}

async fn park_turn_and_kill_server(server: &mut Server) -> Result<ParkedServerState> {
    let response = server
        .rpc(RpcCall::new(
            "runner",
            "alice",
            "SendMessage",
            json!({
                "message": {"messageId": "park-before-kill", "role": "ROLE_USER", "parts": [{"text": "Use the tool"}]}
            }),
        ))
        .await?;
    let task = &response["result"]["task"];
    assert_eq!(
        task["status"]["state"], "TASK_STATE_INPUT_REQUIRED",
        "{response}"
    );
    let id = task["id"].as_str().context("task id")?.to_owned();
    let local = task["contextId"].as_str().context("context id")?.to_owned();
    let session = server.h.session(Some(&local), &alice()).await?;
    let before = await_parked_ownerless(&server.h.store, session.storage_key()).await?;
    assert_eq!(kill_and_reap_server(server)?.signal(), Some(9));
    Ok(ParkedServerState {
        id,
        local,
        session,
        before,
    })
}
async fn assert_restarted_reply_completion(
    restarted: &Server,
    id: &str,
    local: &str,
) -> Result<()> {
    let read = restarted
        .rpc(RpcCall::new(
            "runner",
            "alice",
            "GetTask",
            json!({"id": id}),
        ))
        .await?;
    assert_eq!(
        read["result"]["status"]["state"], "TASK_STATE_INPUT_REQUIRED",
        "{read}"
    );
    let reply = restarted
        .rpc(RpcCall::new(
            "runner",
            "alice",
            "SendMessage",
            json!({
                "message": {"messageId": "after-restart", "role": "ROLE_USER", "contextId": local, "parts": [{"text": "y"}]}
            }),
        ))
        .await?;
    assert_eq!(reply["result"]["task"]["id"], id);
    assert_eq!(
        reply["result"]["task"]["status"]["state"], "TASK_STATE_COMPLETED",
        "{reply}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_sigkill_fleet_restart_while_parked_later_reply_completes() -> Result<()> {
    let mut server = Server::start_script(Script::Tool).await?;
    let state = park_turn_and_kill_server(&mut server).await?;
    let restarted = Server::start_replica(server.h.clone(), "a2a-restarted.log").await?;
    assert_restarted_reply_completion(&restarted, &state.id, &state.local).await?;
    let after = server
        .h
        .store
        .read_context(state.session.storage_key())
        .await?
        .context("context")?;
    assert_preserved_admission(&state.before, &after);
    assert_eq!(server.h.llm.requests.lock().len(), 1);
    Ok(())
}
