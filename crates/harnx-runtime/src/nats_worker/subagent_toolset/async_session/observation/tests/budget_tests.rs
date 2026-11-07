use super::*;
use std::time::Duration;
use tokio::time::{advance, sleep};

#[tokio::test(start_paused = true)]
async fn zero_is_one_bounded_snapshot_with_shared_io_deadline() {
    let budget = Budget::new(0, CancellationToken::new());
    budget.io(sleep(Duration::from_secs(6))).await.unwrap();
    assert_eq!(
        budget.io(sleep(Duration::from_secs(6))).await,
        Err(Stop::Expired)
    );
    assert_eq!(budget.elapsed_ms(), 10_000);
    assert!(budget.is_zero());
    assert_eq!(budget.io(async {}).await, Err(Stop::Expired));
}

#[tokio::test(start_paused = true)]
async fn default_120_and_max_300_use_one_absolute_deadline_across_all_phases() {
    for seconds in [
        super::super::super::DEFAULT_WAIT_SECONDS,
        super::super::super::MAX_WAIT_SECONDS,
    ] {
        let budget = Budget::new(seconds, CancellationToken::new());
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
    let budget = Budget::new(120, cancel.clone());
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
    let budget = Budget::new(120, CancellationToken::new());
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
    let budget = Budget::new(120, CancellationToken::new());
    let result = budget
        .io(async {
            advance(Duration::from_secs(11)).await;
            42
        })
        .await;
    assert_eq!(result, Err(Stop::Unavailable));
    let budget = Budget::new(1, CancellationToken::new());
    let result = budget
        .io(async {
            advance(Duration::from_secs(2)).await;
            42
        })
        .await;
    assert_eq!(result, Err(Stop::Expired));
}
