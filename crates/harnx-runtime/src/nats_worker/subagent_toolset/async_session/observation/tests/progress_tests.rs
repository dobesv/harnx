use super::{fixture::*, *};
use futures_util::{FutureExt, StreamExt};
use harnx_core::{message::MessageRole, session::SessionLogEntry};
use std::time::Duration;

async fn await_repair(f: &Fixture) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if f.activation_count().await? == 2 {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("wait never attempted repair")?
}

#[tokio::test]
async fn pending_wait_observes_durable_visibility_lag_before_terminal_arrives() -> Result<()> {
    let f = Fixture::new().await?;
    let original = f.dispatch().await?;
    let writer = async {
        await_repair(&f).await?;
        f.append(message(
            "looks-final",
            MessageRole::Assistant,
            "not terminal yet",
        ))
        .await?;
        f.append(end(0)).await?;
        let pending = f.poll(json!({"seconds": 1})).await?;
        assert_eq!(pending["status"], "pending");
        f.append(end(original.prompt.user_msg_seq())).await?;
        Ok::<_, anyhow::Error>(())
    };
    let (result, written) = tokio::join!(f.poll(json!({"seconds": 5})), writer);
    written?;
    let result = result?;
    assert_eq!(result["status"], "completed");
    assert_eq!(result["wait_expired"], false);
    assert!(result["output"].to_string().contains("not terminal yet"));
    assert_eq!(f.activation_count().await?, 2);
    Ok(())
}

#[tokio::test]
async fn approval_arriving_during_wait_returns_blocked_without_another_repair_or_cancel(
) -> Result<()> {
    let f = Fixture::new().await?;
    f.dispatch().await?;
    let writer = async {
        await_repair(&f).await?;
        block(&f).await
    };
    let (result, written) = tokio::join!(
        f.poll(json!({"seconds": 5, "include_partial": false})),
        writer
    );
    written?;
    let result = result?;
    assert_eq!(result["status"], "blocked");
    assert_eq!(result["terminal"], false);
    assert!(result["output"].get("events").is_none());
    assert_eq!(f.activation_count().await?, 2);
    assert!(!f
        .log
        .load_events_latest_async()
        .await?
        .iter()
        .any(|(_, e)| matches!(e, SessionLogEntry::Cancel { .. })));
    Ok(())
}

#[tokio::test]
async fn missing_binding_for_existing_raw_prompt_fails_before_scanning_or_repair() -> Result<()> {
    let f = Fixture::new().await?;
    f.dispatch().await?;
    f.append(message("unbound-prompt", MessageRole::User, "unbound"))
        .await?;
    f.append(end(2)).await?;
    let mut args = f.wait_args(json!({"seconds": 120}));
    args["prompt_id"] = json!("unbound-prompt");
    assert!(
        wait(&f.toolset, args, CancellationToken::new(), f.wait_context())
            .await
            .is_err()
    );
    assert_eq!(f.activation_count().await?, 1);
    Ok(())
}

#[tokio::test]
async fn content_mismatch_cannot_be_replaced_by_a_later_correct_copy_or_terminal() -> Result<()> {
    let f = Fixture::new().await?;
    f.dispatch().await?;
    let stream =
        f.js.get_stream(crate::nats_session_log::stream_name_for_session(
            f.log.storage_key(),
        ))
        .await?;
    stream.delete_message(1).await?;
    f.append(message(
        "original-prompt",
        MessageRole::User,
        "forged content",
    ))
    .await?;
    f.append(message("original-prompt", MessageRole::User, "original"))
        .await?;
    f.append(end(3)).await?;
    let result = f.poll(json!({"seconds": 120})).await?;
    assert_eq!(result["status"], "unknown");
    assert_eq!(result["terminal"], false);
    assert_eq!(
        result["output"]["observation"]["error"],
        "prompt_content_mismatch"
    );
    assert!(result["output"].get("events").is_none());
    assert_eq!(f.activation_count().await?, 1);
    Ok(())
}

/// Broker API requests the observer makes against the child's log stream.
/// Each poll issues at least one, so they show when the observer looked.
struct LogReads {
    stream: String,
    requests: async_nats::Subscriber,
}

impl LogReads {
    async fn watch(f: &Fixture) -> Result<Self> {
        let client = f.js.client();
        let stream = crate::nats_session_log::stream_name_for_session(f.log.storage_key());
        let requests = client.subscribe("$JS.API.>").await?;
        // Same-connection ordering: once the probe arrives, the subscription exists.
        let mut probe = client.subscribe("probe.registered").await?;
        client.publish("probe.registered", "".into()).await?;
        tokio::time::timeout(Duration::from_secs(10), probe.next())
            .await
            .context("subscription never registered")?;
        Ok(Self { stream, requests })
    }

    /// Drops what was requested so far.
    fn discard_pending(&mut self) {
        while self.requests.next().now_or_never().is_some() {}
    }

    async fn next_read(&mut self) -> Result<()> {
        loop {
            let request = self
                .requests
                .next()
                .await
                .context("request stream closed")?;
            if request.subject.contains(&self.stream) {
                return Ok(());
            }
        }
    }

    /// Waits until the observer has started a poll after this call: the first
    /// read seen may belong to a poll already under way, but one a full poll
    /// interval later cannot.
    async fn fresh_poll(mut self) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(30), async {
            self.next_read().await?;
            let seen = tokio::time::Instant::now();
            while seen.elapsed() < budget::POLL_INTERVAL {
                self.next_read().await?;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("observer never polled after the compaction")?
    }
}

#[tokio::test]
async fn wait_spanning_compaction_returns_completed() -> Result<()> {
    let f = Fixture::new().await?;
    let original = f.dispatch().await?;
    let writer = async {
        await_repair(&f).await?;
        let mut reads = LogReads::watch(&f).await?;
        compaction_layout(&f).await?;
        // Discard reads that predate the compaction, then require a poll that
        // started after it before the terminal lands.
        reads.discard_pending();
        reads.fresh_poll().await?;
        f.append(end(original.prompt.user_msg_seq())).await?;
        Ok::<_, anyhow::Error>(())
    };
    // Sized for a degraded runner: repair, the compaction appends and a fresh
    // poll cycle all have to fit.
    let (result, written) = tokio::join!(f.poll(json!({"seconds": 30})), writer);
    written?;
    let result = result?;
    assert_eq!(result["status"], "completed");
    assert_eq!(result["wait_expired"], false);
    Ok(())
}

#[tokio::test]
async fn unprovable_live_state_never_repairs_an_unconfirmed_dispatch() -> Result<()> {
    let f = Fixture::new().await?;
    f.dispatch().await?;
    f.append(SessionLogEntry::Rewind { after_seq: 0 }).await?;
    let result = f.poll(json!({"seconds": 2})).await?;
    assert_unprovable(&result, true);
    // Repair is only for a provably pending prompt, so no activation is republished.
    assert_eq!(f.activation_count().await?, 1);
    Ok(())
}

#[tokio::test]
async fn unprovable_live_state_keeps_polling_until_terminal() -> Result<()> {
    let f = Fixture::new().await?;
    let original = f.dispatch().await?;
    f.append(SessionLogEntry::Rewind { after_seq: 0 }).await?;
    let expired = f.poll(json!({"seconds": 1})).await?;
    assert_unprovable(&expired, true);
    assert!(expired["elapsed_ms"].as_u64().unwrap() >= 900);
    let writer = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        f.append(end(original.prompt.user_msg_seq())).await?;
        Ok::<_, anyhow::Error>(())
    };
    let (result, written) = tokio::join!(f.poll(json!({"seconds": 5})), writer);
    written?;
    assert_eq!(result?["status"], "completed");
    Ok(())
}
