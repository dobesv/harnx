use super::*;
use harnx_a2a_server::store::context::ContextSnapshot;
use harnx_runtime::ApprovalRef;

async fn parked(t: &TwoBackends, session: &NatsSession, after: u64) -> Result<ContextSnapshot> {
    tokio::time::timeout(DEADLINE, async {
        loop {
            let context =
                t.h.store
                    .read_context(session.storage_key())
                    .await?
                    .context("context")?;
            let active = context.document.state.active.as_ref().context("active")?;
            if context.document.owner.is_none()
                && active.publication.pending.is_none()
                && active.snapshot.task.status.state == a2a_lf::TaskState::InputRequired
                && active
                    .hitl_checkpoint
                    .as_ref()
                    .is_some_and(|c| c.request.request_seq > after)
            {
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

async fn replica_reply_race(duplicate: bool) -> Result<()> {
    let t = TwoBackends::start_script(Script::SameRoundTools).await?;
    let mut params = first_message();
    params.as_object_mut().unwrap().remove("configuration");
    let initial = rpc(t.a.app.clone(), "alice", "SendMessage", params).await?;
    let task = initial["result"]["task"].clone();
    assert_eq!(
        task["status"]["state"], "TASK_STATE_INPUT_REQUIRED",
        "{initial}"
    );
    let session = t
        .resume(&t.b, task["contextId"].as_str().context("context")?)
        .await?;
    let first = parked(&t, &session, 0).await?;
    let q1 = request(&first).clone();
    // Independent runners/stores are asserted by TwoBackends. Duplicate winner
    // resumes on B, which didn't publish the original challenge on A.
    let (winner, loser) = if duplicate {
        (&t.b, &t.a)
    } else {
        (&t.a, &t.b)
    };
    let winner_message = crate::unary::hitl::reply("winning-reply", &task, "y first audit", true);
    let loser_message = if duplicate {
        winner_message.clone()
    } else {
        crate::unary::hitl::reply("losing-reply", &task, "n must never decide Q2", false)
    };
    let mut bound = loser
        .backend
        .runner
        .fault_hooks()
        .pause(Boundary::HitlReplyBound, 1);
    let delayed = tokio::spawn(rpc(
        loser.app.clone(),
        "alice",
        "SendMessage",
        json!({"message":loser_message}),
    ));
    tokio::time::timeout(DEADLINE, bound.reached()).await?;
    // Both requests are in flight against Q1. Keep loser frozen until winner
    // commits and the real worker publishes Q2 in the same tool round.
    let response = rpc(
        winner.app.clone(),
        "alice",
        "SendMessage",
        json!({"message":winner_message}),
    )
    .await?;
    assert_eq!(
        response["result"]["task"]["status"]["state"], "TASK_STATE_INPUT_REQUIRED",
        "{response}"
    );
    assert_eq!(response["result"]["task"]["id"], task["id"]);
    let second = parked(&t, &session, q1.request_seq).await?;
    let q2 = request(&second);
    assert_eq!(q2.tool_round_seq, q1.tool_round_seq);
    assert_eq!(q2.invocation_id, q1.invocation_id);
    assert_ne!(q2.tool_call_id, q1.tool_call_id);
    drop(bound);
    let delayed = tokio::time::timeout(DEADLINE, delayed).await???;
    if duplicate {
        assert_eq!(delayed["result"]["task"]["id"], task["id"], "{delayed}");
        assert_eq!(
            delayed["result"]["task"]["status"]["state"],
            "TASK_STATE_INPUT_REQUIRED"
        );
    } else {
        assert_eq!(delayed["error"]["code"], -32602, "{delayed}");
    }
    let log = NatsSessionLog::new(t.h.jetstream.clone(), session.storage_key());
    let entries = log.load_events_latest_required_async().await?;
    let decisions: Vec<_> = entries
        .iter()
        .filter_map(|(_, e)| match e {
            SessionLogEntry::HitlApprovalDecision {
                tool_call_id,
                approved,
                note,
                ..
            } => Some((tool_call_id.as_str(), *approved, note.as_deref())),
            _ => None,
        })
        .collect();
    assert_eq!(
        decisions,
        vec![(q1.tool_call_id.as_str(), true, Some("first audit"))]
    );
    let unchanged = parked(&t, &session, q1.request_seq).await?;
    assert_eq!(request(&unchanged), q2);
    let active = unchanged.document.state.active.as_ref().unwrap();
    assert!(active.hitl_checkpoint.as_ref().unwrap().reply.is_none());
    let history = active.snapshot.task.history.as_ref().unwrap();
    assert_eq!(
        history
            .iter()
            .filter(|m| m.message_id == "winning-reply")
            .count(),
        1
    );
    assert!(!history.iter().any(|m| m.message_id == "losing-reply"));
    assert_eq!(prompts(&log).await?.len(), 1);
    assert_eq!(
        t.h.llm.requests.lock().len(),
        1,
        "Q2 must not require another model round"
    );

    let final_response = rpc(
        loser.app.clone(),
        "alice",
        "SendMessage",
        json!({"message":crate::unary::hitl::reply("second-reply", &task, "y", false)}),
    )
    .await?;
    assert_eq!(final_response["result"]["task"]["id"], task["id"]);
    assert_eq!(
        final_response["result"]["task"]["status"]["state"], "TASK_STATE_COMPLETED",
        "{final_response}"
    );
    let final_record = t.h.task(task["id"].as_str().unwrap()).await?;
    let original = &first.document.state.active.as_ref().unwrap().snapshot;
    assert_eq!(final_record.execution_id, original.execution_id);
    assert_eq!(final_record.user_msg_seq, original.user_msg_seq);
    assert_eq!(prompts(&log).await?.len(), 1);
    assert_eq!(
        log.load_events_latest_required_async()
            .await?
            .iter()
            .filter(|(_, e)| matches!(e, SessionLogEntry::HitlApprovalDecision { .. }))
            .count(),
        2
    );
    assert_eq!(t.h.llm.requests.lock().len(), 2);
    // Durable mappings work on either replica even after the terminal boundary.
    for replica in [&t.a, &t.b] {
        let retry = rpc(
            replica.app.clone(),
            "alice",
            "SendMessage",
            json!({"message":winner_message}),
        )
        .await?;
        assert_eq!(retry["result"]["task"]["id"], task["id"], "{retry}");
        assert_eq!(
            retry["result"]["task"]["status"]["state"],
            "TASK_STATE_COMPLETED"
        );
    }
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
