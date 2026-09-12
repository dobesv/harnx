use super::*;

#[test]
fn lifecycle_state_assessment_matches_tartarus_and_represents_hibernation() {
    let pending = pending_record("pending");
    let pending_status = assess(&pending);
    assert_status(&pending_status, ("pending", false, false, false));

    let ready_status = assess(&record("ready", Some(1), None, Utc::now()));
    assert_status(&ready_status, ("ready", true, true, false));

    for (reason, message) in [
        ("CreateError", ""),
        ("Pending", "failed to provision"),
        ("Pending", "access denied"),
    ] {
        let mut failed = pending_record("failed");
        failed.conditions = vec![SandboxCondition {
            kind: "Ready".to_string(),
            status: "False".to_string(),
            reason: reason.to_string(),
            message: message.to_string(),
        }];
        let status = assess(&failed);
        assert_status(&status, ("error", false, true, true));
    }

    let hibernated = assess(&record("sleeping", Some(0), None, Utc::now()));
    assert_status(&hibernated, ("hibernated", false, true, false));

    assert_status(&deleted_status("gone"), ("deleted", false, true, false));
}

#[tokio::test]
async fn status_observes_hibernation_without_waking_or_touching_activity() {
    let api = Arc::new(MockApi::default());
    api.insert(record("claim-1", Some(0), None, Utc::now()));
    let status = test_manager(api.clone())
        .status("claim-1", None, &CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(status.state, "hibernated");
    assert!(status.terminal);
    let state = api.state.lock();
    assert!(state.replica_updates.is_empty());
    assert!(state.activity_updates.is_empty());
    assert!(state.shutdown_updates.is_empty());
}

#[tokio::test]
async fn status_wait_retries_transient_api_errors() {
    let api = Arc::new(MockApi::default());
    api.insert(record("claim-ready", Some(1), None, Utc::now()));
    api.state.lock().get_errors_remaining = 2;

    let status = test_manager(api)
        .status(
            "claim-ready",
            Some(Duration::from_secs(1)),
            &CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(status.state, "ready");
    assert!(!status.timed_out);
}

#[tokio::test]
async fn status_wait_times_out_with_the_last_observed_state() {
    let api = Arc::new(MockApi::default());
    api.insert(pending_record("claim-pending"));
    let status = test_manager(api)
        .status(
            "claim-pending",
            Some(Duration::from_millis(3)),
            &CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(status.state, "pending");
    assert!(status.timed_out);
    assert!(!status.terminal);
}

#[tokio::test]
async fn status_wait_times_out_after_only_transient_api_errors() {
    let api = Arc::new(MockApi::default());
    api.state.lock().get_errors_remaining = usize::MAX;
    let status = test_manager(api)
        .status(
            "claim-unknown",
            Some(Duration::from_millis(3)),
            &CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(status.sandbox_id, "claim-unknown");
    assert_eq!(status.state, "pending");
    assert!(status.timed_out);
}
