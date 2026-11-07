use super::{fixture::*, *};
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
        let pending = f.poll(json!({"seconds": 0})).await?;
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
