use super::*;
use harnx_runtime::interaction_checkpoint::{
    round_start_key, round_started, RoundStart, ROUND_START_BUCKET,
};

#[test]
fn interaction_owned_guard_round_start_key_is_namespaced_and_stable() {
    assert_eq!(
        round_start_key("abc", 42),
        "sessions.ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad.rounds.42.start"
    );
    assert_ne!(
        round_start_key("a:session", 42),
        round_start_key("b:session", 42)
    );
    assert_ne!(
        round_start_key("a:session", 42),
        round_start_key("a:session", 43)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interaction_owned_guard_checkpoint_identity_errors_are_not_absence() -> Result<()> {
    let fixture = Fixture::new(Case::RoundStart).await?;
    let entries = fixture.log.load_events_latest_async().await?;
    let round = entries
        .iter()
        .find_map(|(seq, entry)| matches!(entry, SessionLogEntry::ToolCalls { .. }).then_some(*seq))
        .context("tool round")?;
    let storage = fixture.log.storage_key();
    let store = fixture.js.get_key_value(ROUND_START_BUCKET).await?;
    let key = round_start_key(storage, round);
    assert!(round_started(&fixture.js, storage, round).await?);
    let valid = serde_json::to_value(RoundStart {
        version: 1,
        session_storage_key: storage.into(),
        tool_round_seq: round,
        fence_token: 1,
    })?;
    for (field, value) in [
        ("version", json!(2)),
        ("session_storage_key", json!("another:session")),
        ("tool_round_seq", json!(round + 1)),
    ] {
        let mut record = valid.clone();
        record[field] = value;
        store.put(&key, serde_json::to_vec(&record)?.into()).await?;
        assert!(
            round_started(&fixture.js, storage, round).await.is_err(),
            "{field}"
        );
        fixture.direct_run().await?;
        assert_eq!(fixture.log.load_events_latest_async().await?, entries);
    }
    assert_eq!(fixture.models.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.tools.load(Ordering::SeqCst), 0);
    Ok(())
}
