use super::*;

#[tokio::test]
async fn release_hibernates_or_destroys_the_claim() {
    let api = Arc::new(MockApi::default());
    api.insert(record("claim-release", Some(1), None, Utc::now()));
    let manager = test_manager(api.clone());

    assert_eq!(
        manager
            .release("claim-release", false, &CancellationToken::new())
            .await
            .unwrap(),
        "hibernated"
    );
    assert_eq!(
        api.state.lock().replica_updates,
        [("claim-release".to_string(), 0)]
    );

    assert_eq!(
        manager
            .release("claim-release", true, &CancellationToken::new())
            .await
            .unwrap(),
        "destroyed"
    );
    assert_eq!(api.state.lock().deletes, ["claim-release"]);
}

#[tokio::test]
async fn create_uses_a_stable_dns_safe_name_for_retries() {
    let api = Arc::new(MockApi::default());
    let manager = test_manager(api.clone());

    let first = manager
        .create("CALL_123/a", None, &CancellationToken::new())
        .await
        .unwrap();
    let second = manager
        .create("CALL_123/a", None, &CancellationToken::new())
        .await
        .unwrap();

    assert!(first.starts_with("sandbox-"));
    assert_eq!(first.len(), 40);
    assert_eq!(second, first);
    assert_eq!(api.state.lock().creates, [first.clone(), first]);
}
