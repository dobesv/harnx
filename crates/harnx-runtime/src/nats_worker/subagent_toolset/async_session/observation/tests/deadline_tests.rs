//! A wait stops a minute before the caller's run deadline, and still reports
//! what the child is doing when that bound is already reached.
use super::{fixture::*, *};
use harnx_core::message::MessageRole;
use std::time::Duration;

/// The wait context whose caller's run deadline is `in_secs` from now
/// (negative: already past).
fn context_with_deadline(f: &Fixture, in_secs: i64) -> ToolInvocationContext {
    let deadline = chrono::Utc::now() + chrono::Duration::seconds(in_secs);
    ToolInvocationContext {
        run_context: Some(harnx_toolset::AutonomousRunContext {
            snapshot: json!({"deadline": deadline.to_rfc3339()}),
            started_at_ms: 0,
        }),
        ..f.wait_context()
    }
}

async fn wait_under_deadline(f: &Fixture, options: Value, in_secs: i64) -> Result<Value> {
    let context = context_with_deadline(f, in_secs);
    let args = f.wait_args(options);
    Ok(wait(&f.toolset, args, CancellationToken::new(), context).await?)
}

/// The wait reached the caller's bound, not its own limit, and reported
/// `status` and whether it was terminal. A terminal result names no stop:
/// the work was done, so nothing ended early.
fn assert_caller_deadline(result: &Value, status: &str, max_ms: u64) {
    let seen = (
        result["stopped_by"].as_str(),
        result["wait_expired"].as_bool(),
        result["status"].as_str(),
        result["terminal"].as_bool(),
    );
    let terminal = status == "completed";
    let stopped_by = (!terminal).then_some("caller_deadline");
    assert_eq!(
        seen,
        (stopped_by, Some(false), Some(status), Some(terminal)),
        "{result}"
    );
    // Sized for a degraded runner; the bound itself is about a second.
    assert!(result["elapsed_ms"].as_u64().unwrap() < max_ms, "{result}");
}

#[tokio::test]
async fn caller_deadline_a_minute_away_ends_forever_and_long_waits_early() -> Result<()> {
    let f = Fixture::new().await?;
    f.dispatch().await?;
    for (options, in_secs) in [
        (json!({"forever": true}), 61),
        (json!({"seconds": 240}), 62),
    ] {
        let result = wait_under_deadline(&f, options, in_secs).await?;
        assert_caller_deadline(&result, "pending", 20_000);
    }
    Ok(())
}

#[tokio::test]
async fn caller_deadline_already_past_still_reports_a_pending_child() -> Result<()> {
    let f = Fixture::new().await?;
    f.dispatch().await?;
    let result = wait_under_deadline(&f, json!({"forever": true}), -30).await?;
    assert_caller_deadline(&result, "pending", 10_000);
    assert_eq!(result["prompt_id"], "original-prompt");
    Ok(())
}

#[tokio::test]
async fn caller_deadline_already_past_still_reports_a_finished_child() -> Result<()> {
    let f = Fixture::new().await?;
    let prompt = f.dispatch().await?;
    f.append(message("answer", MessageRole::Assistant, "done"))
        .await?;
    f.append(end(prompt.prompt.user_msg_seq())).await?;
    let result = wait_under_deadline(&f, json!({"seconds": 240}), 10).await?;
    assert_caller_deadline(&result, "completed", 10_000);
    Ok(())
}

#[tokio::test]
async fn caller_deadline_absent_leaves_forever_waiting() -> Result<()> {
    let f = Fixture::new().await?;
    f.dispatch().await?;
    let args = f.wait_args(json!({"forever": true}));
    let pending = wait(&f.toolset, args, CancellationToken::new(), f.wait_context());
    let outcome = tokio::time::timeout(Duration::from_secs(3), pending).await;
    assert!(outcome.is_err(), "no run context must mean no clamp");
    Ok(())
}
