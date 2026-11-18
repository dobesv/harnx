use super::*;
use crate::nats_worker::tests::spawn_test_nats;
use serde_json::{json, Value};

#[tokio::test]
async fn chunked_checkpoints_survive_reopen_and_reject_foreign_refs_and_corruption() -> Result<()> {
    let (url, _broker, _directory) = spawn_test_nats()
        .await
        .context("NATS test binary is required")?;
    let client = async_nats::connect(url).await?;
    let js = jetstream::new(client);
    // T2.1 deployments may have created this bucket with default message limits.
    js.create_key_value(kv::Config {
        bucket: ROUND_START_BUCKET.into(),
        ..Default::default()
    })
    .await?;
    let store = CheckpointStore::ensure(&js, 1).await?;
    let start = RoundStart {
        version: 1,
        session_storage_key: "agent/session".into(),
        tool_round_seq: 4,
        fence_token: 9,
    };
    store.start_round(&start).await?;
    let mut resumed = start.clone();
    resumed.fence_token = 10;
    store.start_round(&resumed).await?;
    let saved: RoundStart = serde_json::from_slice(
        &store
            .get(&round_start_key("agent/session", 4))
            .await?
            .unwrap(),
    )?;
    assert_eq!(
        saved, start,
        "resume must not replace original round ownership proof"
    );
    let prefix = records::call_prefix("agent/session", 4, "a");
    let value = json!({"stash": "x".repeat(CHUNK_BYTES * 2 + 37)});
    let reference = store.put(&prefix, "checkpoints.one", &value).await?;
    drop(store);
    let reopened = CheckpointStore::ensure(&js, 1).await?;
    assert_eq!(reopened.read::<Value>(&prefix, &reference).await?, value);
    assert!(reopened
        .read::<Value>(&records::call_prefix("agent/session", 4, "b"), &reference)
        .await
        .is_err());
    reopened
        .kv
        .put(format!("{reference}.chunks.0"), "corrupt".into())
        .await?;
    assert!(reopened
        .read::<Value>(&prefix, &reference)
        .await
        .unwrap_err()
        .to_string()
        .contains("checksum"));
    Ok(())
}

#[tokio::test]
async fn failed_put_releases_reservation_and_removes_its_own_chunks() -> Result<()> {
    let (url, _broker, _directory) = spawn_test_nats()
        .await
        .context("NATS test binary is required")?;
    let js = jetstream::new(async_nats::connect(url).await?);
    let store = CheckpointStore::ensure(&js, 1).await?;
    let prefix = records::call_prefix("agent/session", 1, "a");
    let key = format!("{prefix}checkpoints.two");
    // Occupy chunk 1 so the second chunk create fails after chunk 0 committed,
    // imitating a durable-write failure once the reservation was taken.
    store
        .kv
        .put(&chunk_key(&key, 1), b"taken".to_vec().into())
        .await?;
    let value = json!({"stash": "x".repeat(CHUNK_BYTES + 1)});
    assert!(store.put(&prefix, "checkpoints.two", &value).await.is_err());
    assert!(
        store.get(&chunk_key(&key, 0)).await?.is_none(),
        "the chunk the failed put committed must be removed"
    );
    let retained: Option<u64> = store
        .get(&format!("{prefix}retained_bytes"))
        .await?
        .map(|bytes| serde_json::from_slice(&bytes))
        .transpose()?;
    assert_eq!(
        retained,
        Some(0),
        "the failed put's reservation must be released"
    );
    // Accounting stays usable: a later put succeeds and counts only itself.
    let ok = json!({"stash": "ok"});
    let reference = store.put(&prefix, "checkpoints.three", &ok).await?;
    let used: u64 = serde_json::from_slice(
        &store
            .get(&format!("{prefix}retained_bytes"))
            .await?
            .context("reservation key must exist after a successful put")?,
    )?;
    assert_eq!(used as usize, serde_json::to_vec(&ok)?.len());
    assert_eq!(store.read::<Value>(&prefix, &reference).await?, ok);
    Ok(())
}

#[tokio::test]
async fn retained_byte_reservations_survive_reopen_and_incomplete_uploads_fail_closed() -> Result<()>
{
    let (url, _broker, _directory) = spawn_test_nats()
        .await
        .context("NATS test binary is required")?;
    let js = jetstream::new(async_nats::connect(url).await?);
    let store = CheckpointStore::ensure(&js, 1).await?;
    let prefix = records::call_prefix("agent/session", 1, "a");
    store.reserve(&prefix, MAX_CALL_BYTES).await?;
    let reopened = CheckpointStore::ensure(&js, 1).await?;
    assert!(reopened
        .put(&prefix, "extra", &json!(null))
        .await
        .unwrap_err()
        .to_string()
        .contains("64 MiB"));
    let other = records::call_prefix("agent/session", 1, "b");
    let key = format!("{other}checkpoints.partial");
    let manifest = Blob {
        version: 1,
        bytes: 8,
        chunks: 1,
        sha256: "unused".into(),
    };
    reopened
        .kv
        .put(&key, serde_json::to_vec(&manifest)?.into())
        .await?;
    assert!(reopened
        .read::<Value>(&other, &key)
        .await
        .unwrap_err()
        .to_string()
        .contains("missing interaction checkpoint chunk"));
    reopened
        .purge_prefix(&records::round_prefix("agent/session", 1))
        .await?;
    assert!(reopened.get(&key).await?.is_none());
    assert!(reopened
        .get(&format!("{prefix}retained_bytes"))
        .await?
        .is_none());
    Ok(())
}
