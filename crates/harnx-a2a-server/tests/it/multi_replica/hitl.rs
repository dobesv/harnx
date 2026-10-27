use super::*;
use harnx_a2a_server::store::context::ContextSnapshot;
use harnx_runtime::ApprovalRef;

use crate::multi_replica::hitl_state::is_parked_waiting;

mod support;
use support::*;

async fn parked(t: &TwoBackends, session: &NatsSession, after: u64) -> Result<ContextSnapshot> {
    tokio::time::timeout(DEADLINE, async {
        loop {
            let context =
                t.h.store
                    .read_context(session.storage_key())
                    .await?
                    .context("context")?;
            if is_parked_waiting(&context, after) {
                return Ok(context);
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .context("park deadline")?
}

fn request(context: &ContextSnapshot) -> &ApprovalRef {
    &context
        .document
        .state
        .active
        .as_ref()
        .unwrap()
        .hitl_checkpoint
        .as_ref()
        .unwrap()
        .request
}

struct InitialRaceState {
    t: TwoBackends,
    task: Value,
    session: NatsSession,
    first: ContextSnapshot,
    q1: ApprovalRef,
}

async fn setup_race() -> Result<InitialRaceState> {
    let t = TwoBackends::start_script(Script::SameRoundTools).await?;
    let (task, session, first) = start_race(&t).await?;
    let q1 = request(&first).clone();
    Ok(InitialRaceState {
        t,
        task,
        session,
        first,
        q1,
    })
}

struct ReplicaPair<'a> {
    winner: &'a Replica,
    loser: &'a Replica,
    winner_message: Value,
    loser_message: Value,
}

fn select_replicas<'a>(t: &'a TwoBackends, task: &Value, duplicate: bool) -> ReplicaPair<'a> {
    let (winner, loser) = if duplicate {
        (&t.b, &t.a)
    } else {
        (&t.a, &t.b)
    };
    let winner_message = crate::unary::hitl::reply("winning-reply", task, "y first audit", true);
    let loser_message = if duplicate {
        winner_message.clone()
    } else {
        crate::unary::hitl::reply("losing-reply", task, "n must never decide Q2", false)
    };
    ReplicaPair {
        winner,
        loser,
        winner_message,
        loser_message,
    }
}

async fn drive_winner_step(winner: &Replica, task: &Value, winner_message: &Value) -> Result<()> {
    let response = rpc(
        winner.app.clone(),
        "alice",
        "SendMessage",
        json!({"message": winner_message}),
    )
    .await?;
    assert_eq!(
        response["result"]["task"]["status"]["state"], "TASK_STATE_INPUT_REQUIRED",
        "{response}"
    );
    assert_eq!(response["result"]["task"]["id"], task["id"]);
    Ok(())
}

async fn observe_second_gate(
    t: &TwoBackends,
    session: &NatsSession,
    q1: &ApprovalRef,
) -> Result<(ContextSnapshot, ApprovalRef)> {
    let second = parked(t, session, q1.request_seq).await?;
    let q2 = request(&second).clone();
    assert_eq!(q2.tool_round_seq, q1.tool_round_seq);
    assert_eq!(q2.invocation_id, q1.invocation_id);
    assert_ne!(q2.tool_call_id, q1.tool_call_id);
    Ok((second, q2))
}

async fn await_delayed_reply(
    delayed: tokio::task::JoinHandle<Result<Value>>,
    task: &Value,
    duplicate: bool,
) -> Result<()> {
    let delayed = tokio::time::timeout(DEADLINE, delayed).await???;
    assert_delayed_reply(&delayed, task, duplicate);
    Ok(())
}

async fn finish_race_loser(
    t: &TwoBackends,
    loser: &Replica,
    comp: RaceCompletion<'_>,
    winner_message: &Value,
) -> Result<()> {
    let task = comp.task;
    complete_on_loser(t, loser, comp).await?;
    assert_terminal_retries(t, task, winner_message).await?;
    Ok(())
}

async fn replica_reply_race(duplicate: bool) -> Result<()> {
    let state = setup_race().await?;
    let pair = select_replicas(&state.t, &state.task, duplicate);
    let mut bound = pair
        .loser
        .backend
        .runner
        .fault_hooks()
        .pause(Boundary::HitlReplyBound, 1);
    let delayed = tokio::spawn(rpc(
        pair.loser.app.clone(),
        "alice",
        "SendMessage",
        json!({"message": pair.loser_message}),
    ));
    tokio::time::timeout(DEADLINE, bound.reached()).await?;
    drive_winner_step(pair.winner, &state.task, &pair.winner_message).await?;
    let (_second, q2) = observe_second_gate(&state.t, &state.session, &state.q1).await?;
    drop(bound);
    await_delayed_reply(delayed, &state.task, duplicate).await?;
    let log = assert_undecided_gate(&state.t, &state.session, &state.q1, &q2).await?;
    finish_race_loser(
        &state.t,
        pair.loser,
        RaceCompletion {
            task: &state.task,
            first: &state.first,
            log: &log,
        },
        &pair.winner_message,
    )
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hitl_duplicate_across_independent_replicas_resumes_once() -> Result<()> {
    tokio::time::timeout(DEADLINE, replica_reply_race(true)).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hitl_conflicting_replicas_first_wins_loser_cannot_answer_next_gate() -> Result<()> {
    tokio::time::timeout(DEADLINE, replica_reply_race(false)).await?
}
