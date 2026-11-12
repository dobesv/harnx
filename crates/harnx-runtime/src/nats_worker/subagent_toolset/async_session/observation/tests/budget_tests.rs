use super::*;
use std::time::Duration;
use tokio::time::{advance, sleep, Instant};

#[tokio::test(start_paused = true)]
async fn snapshot_is_one_bounded_pass_with_shared_io_deadline() {
    let budget = Budget::snapshot(CancellationToken::new());
    budget.io(sleep(Duration::from_secs(6))).await.unwrap();
    assert_eq!(
        budget.io(sleep(Duration::from_secs(6))).await,
        Err(Stop::Expired)
    );
    assert_eq!(budget.elapsed_ms(), 10_000);
    assert_eq!(budget.io(async {}).await, Err(Stop::Expired));
}

#[tokio::test(start_paused = true)]
async fn default_and_max_240_use_one_absolute_deadline_across_all_phases() {
    for seconds in [
        super::super::super::WAIT_DEFAULT_SECS,
        super::super::super::WAIT_MAX_SECS,
    ] {
        let budget = Budget::new(WaitLimit::Seconds(seconds), BudgetStops::default());
        // Authorization/history, repair and presentation all share one clock.
        budget.io(sleep(Duration::from_secs(8))).await.unwrap();
        budget.io(sleep(Duration::from_secs(8))).await.unwrap();
        advance(Duration::from_secs(seconds - 17)).await;
        assert_eq!(
            budget.io(sleep(Duration::from_secs(2))).await,
            Err(Stop::Expired)
        );
        assert_eq!(budget.elapsed_ms(), seconds * 1000);
        assert_eq!(budget.poll().await, Err(Stop::Expired));
    }
}

#[tokio::test(start_paused = true)]
async fn io_cap_and_cancel_do_not_renew_the_observation_deadline() {
    let cancel = CancellationToken::new();
    let budget = Budget::new(
        WaitLimit::Seconds(120),
        BudgetStops {
            cancel: cancel.clone(),
            ..Default::default()
        },
    );
    assert_eq!(
        budget.io(std::future::pending::<()>()).await,
        Err(Stop::Unavailable)
    );
    assert_eq!(budget.elapsed_ms(), 10_000);
    cancel.cancel();
    assert_eq!(
        budget
            .io(async { panic!("cancelled observer did work") })
            .await,
        Err(Stop::Cancelled)
    );
    assert_eq!(budget.elapsed_ms(), 10_000);
}

#[tokio::test(start_paused = true)]
async fn terminal_proof_and_handle_survive_presentation_timeout_under_same_budget() {
    let handle = PromptHandle {
        session_id: "child".into(),
        prompt_id: "original".into(),
    };
    let budget = Budget::new(WaitLimit::Seconds(120), BudgetStops::default());
    let mut frame = Frame::new(&handle, 1024);
    frame.result.status = SessionPromptStatus::Completed;
    frame.result.terminal = true;
    advance(Duration::from_secs(120)).await;
    frame.stopped(Stop::Expired);
    let result = frame.finish(&budget);
    assert_eq!(result["status"], "completed");
    assert_eq!(result["terminal"], true);
    assert_eq!(result["wait_expired"], false);
    assert_eq!(result["prompt_id"], "original");
    assert!(fixture::serialized(&result) <= 1024);
}

#[tokio::test(start_paused = true)]
async fn ready_decode_future_cannot_hide_soft_deadline_or_bounded_io_overrun() {
    let budget = Budget::new(WaitLimit::Seconds(120), BudgetStops::default());
    let result = budget
        .io(async {
            advance(Duration::from_secs(11)).await;
            42
        })
        .await;
    assert_eq!(result, Err(Stop::Unavailable));
    let budget = Budget::new(WaitLimit::Seconds(1), BudgetStops::default());
    let result = budget
        .io(async {
            advance(Duration::from_secs(2)).await;
            42
        })
        .await;
    assert_eq!(result, Err(Stop::Expired));
}

#[tokio::test(start_paused = true)]
async fn forever_has_no_deadline_and_stops_on_cancel() {
    let cancel = CancellationToken::new();
    let budget = Budget::new(
        WaitLimit::Forever,
        BudgetStops {
            cancel: cancel.clone(),
            ..Default::default()
        },
    );
    for _ in 0..3 {
        budget.io(sleep(Duration::from_secs(8))).await.unwrap();
    }
    advance(Duration::from_secs(100_000)).await;
    assert_eq!(budget.check(), Ok(()));
    cancel.cancel();
    assert_eq!(budget.poll().await, Err(Stop::Cancelled));
}

/// A new message ends the wait between observations: an observation in
/// progress still completes, so the result is never a read cut short.
#[tokio::test(start_paused = true)]
async fn new_message_ends_the_poll_but_not_the_observation_in_progress() {
    let stops = BudgetStops::default();
    let new_message = stops.new_message.clone();
    let budget = Budget::new(WaitLimit::Forever, stops);
    new_message.cancel();
    assert_eq!(budget.io(async { 7 }).await, Ok(7));
    assert_eq!(budget.poll().await, Err(Stop::NewMessage));
    assert_eq!(budget.elapsed_ms(), 0);
}

fn caller_stops(bound: Duration) -> BudgetStops {
    BudgetStops {
        caller_deadline: Some(Instant::now() + bound),
        ..Default::default()
    }
}

#[tokio::test(start_paused = true)]
async fn caller_deadline_bounds_forever_and_long_waits_with_its_own_stop() {
    for limit in [WaitLimit::Forever, WaitLimit::Seconds(240)] {
        let budget = Budget::new(limit, caller_stops(Duration::from_secs(2)));
        assert_eq!(budget.poll().await, Ok(()));
        advance(Duration::from_secs(2)).await;
        assert_eq!(budget.poll().await, Err(Stop::CallerDeadline));
        assert_eq!(budget.elapsed_ms(), 2_500);
    }
}

#[tokio::test(start_paused = true)]
async fn caller_deadline_already_reached_stops_at_once_and_a_nearer_limit_still_expires() {
    let budget = Budget::new(WaitLimit::Forever, caller_stops(Duration::ZERO));
    assert_eq!(budget.poll().await, Err(Stop::CallerDeadline));
    assert_eq!(budget.elapsed_ms(), 0);

    let budget = Budget::new(WaitLimit::Seconds(1), caller_stops(Duration::from_secs(50)));
    advance(Duration::from_secs(1)).await;
    assert_eq!(budget.poll().await, Err(Stop::Expired));
}
