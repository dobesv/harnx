use super::*;
use harnx_toolset::{AutonomousRunContext, ToolInvocationContext};

fn wait_args(tool: &str) -> Value {
    match tool {
        "wait" => json!({ "seconds": 3600 }),
        "wait_until" => target_args(chrono::Duration::hours(24)),
        _ => unreachable!(),
    }
}

fn target_args(ahead: chrono::Duration) -> Value {
    json!({
        "time": (Utc::now() + ahead).format("%Y-%m-%dT%H:%M:%S").to_string(),
        "timezone": "UTC",
    })
}

fn deadline_context(ahead_seconds: i64) -> ToolInvocationContext {
    ToolInvocationContext {
        run_context: Some(AutonomousRunContext {
            snapshot: json!({
                "deadline": (Utc::now() + chrono::Duration::seconds(ahead_seconds)).to_rfc3339(),
            }),
            started_at_ms: 0,
        }),
        ..Default::default()
    }
}

async fn invoke(tool: &str, args: Value, context: ToolInvocationContext) -> (Value, Duration) {
    let started = Instant::now();
    let result = TimeToolset::new()
        .invoke_with_context(ToolInvocation {
            tool: tool.into(),
            args,
            context,
            cancel: CancellationToken::new(),
        })
        .await
        .expect("wait returns a normal result");
    (result, started.elapsed())
}

#[tokio::test(start_paused = true)]
async fn waits_stop_before_caller_deadline() {
    for tool in ["wait", "wait_until"] {
        // wait_bound leaves a minute for the caller to report back.
        let (result, elapsed) = invoke(tool, wait_args(tool), deadline_context(90)).await;
        assert_eq!(result["stopped_by"], "caller_deadline");
        assert!(elapsed >= Duration::from_secs(29), "{tool}: {elapsed:?}");
        assert!(elapsed <= Duration::from_secs(31), "{tool}: {elapsed:?}");
        assert!(result["started_at"].is_string() && result["ended_at"].is_string());
        if tool == "wait_until" {
            assert!(result["waited_seconds"].as_f64().unwrap() < 31.0);
        } else {
            assert!(result["message"]
                .as_str()
                .unwrap()
                .contains("run deadline is near"));
        }
    }
}

#[tokio::test(start_paused = true)]
async fn waits_return_immediately_for_past_caller_bound() {
    for tool in ["wait", "wait_until"] {
        // Test both a passed run deadline and one already inside the margin.
        for ahead in [-1, 30] {
            let (result, elapsed) = invoke(tool, wait_args(tool), deadline_context(ahead)).await;
            assert_eq!(result["stopped_by"], "caller_deadline");
            assert_eq!(elapsed, Duration::ZERO);
            if tool == "wait_until" {
                assert_eq!(result["waited_seconds"], 0.0);
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn waits_complete_full_duration_without_caller_deadline() {
    for tool in ["wait", "wait_until"] {
        for context in [
            ToolInvocationContext::default(),
            ToolInvocationContext {
                run_context: Some(AutonomousRunContext {
                    snapshot: json!({}),
                    started_at_ms: 0,
                }),
                ..Default::default()
            },
        ] {
            let (result, elapsed) = invoke(tool, wait_args(tool), context).await;
            assert!(result.get("stopped_by").is_none());
            assert!(result["started_at"].is_string() && result["ended_at"].is_string());
            if tool == "wait" {
                assert_eq!(elapsed, Duration::from_secs(3600));
                assert_eq!(result["message"], "Waited 3600.0 seconds");
            } else {
                assert!(
                    (Duration::from_secs(86_398)..=Duration::from_secs(86_400)).contains(&elapsed)
                );
                let waited = result["waited_seconds"].as_f64().unwrap();
                assert!((waited - elapsed.as_secs_f64()).abs() < 0.002);
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn waits_complete_when_own_duration_precedes_caller_bound() {
    for (tool, args) in [
        ("wait", json!({ "seconds": 5 })),
        ("wait_until", target_args(chrono::Duration::seconds(5))),
    ] {
        let (result, elapsed) = invoke(tool, args, deadline_context(90)).await;
        assert!(result.get("stopped_by").is_none());
        assert!((Duration::from_secs(4)..=Duration::from_secs(5)).contains(&elapsed));
    }
}

#[tokio::test(start_paused = true)]
async fn new_message_takes_precedence_over_caller_deadline() {
    for tool in ["wait", "wait_until"] {
        let context = deadline_context(-1);
        context.new_message.cancel();
        let (result, elapsed) = invoke(tool, wait_args(tool), context).await;
        assert_eq!(result["stopped_by"], "new_message");
        assert_eq!(elapsed, Duration::ZERO);
    }
}

#[tokio::test(start_paused = true)]
async fn cancellation_still_fails_with_past_caller_deadline() {
    for tool in ["wait", "wait_until"] {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = TimeToolset::new()
            .invoke_with_context(ToolInvocation {
                tool: tool.into(),
                args: wait_args(tool),
                context: deadline_context(-1),
                cancel,
            })
            .await;
        assert!(matches!(result, Err(ToolInvokeError::Fatal(_))));
    }
}
