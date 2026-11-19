use super::*;
use std::time::Duration;

struct Server {
    child: crate::nats_worker::tests::TestNatsServer,
    _directory: tempfile::TempDir,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[tokio::test]
async fn confirmation_waits_past_rpc_and_former_approval_deadlines() -> Result<()> {
    let Some((url, child, directory)) = crate::nats_worker::tests::spawn_test_nats().await else {
        return Ok(());
    };
    let _server = Server {
        child,
        _directory: directory,
    };
    let client = async_nats::ConnectOptions::new()
        .request_timeout(Some(Duration::from_millis(250)))
        .connect(url)
        .await?;
    let subject = client.new_inbox();
    let mut frontend = client.subscribe(subject.clone()).await?;
    client.flush().await?;

    let abort = crate::utils::create_abort_signal();
    let mut decision = tokio_test::task::spawn(request_confirmation(
        &client,
        &subject,
        b"approval request".to_vec(),
        &abort,
    ));
    assert!(decision.poll().is_pending());
    let request = tokio::time::timeout(Duration::from_secs(5), frontend.next())
        .await?
        .context("frontend did not receive approval request")?;

    // Advance only after the real broker delivered the request. Poll the
    // production wait after both deadlines, without a wall-clock delay.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(24 * 60 * 60)).await;
    assert!(
        decision.poll().is_pending(),
        "human approval must not expire"
    );
    tokio::time::resume();

    client
        .publish(
            request.reply.context("reply subject")?,
            serde_json::to_vec(&ToolConfirmationResponse {
                approved: Some(true),
            })?
            .into(),
        )
        .await?;
    let response = tokio::time::timeout(Duration::from_secs(5), decision)
        .await?
        .context("confirmation was cancelled")??;
    assert_eq!(
        serde_json::from_slice::<ToolConfirmationResponse>(&response.payload)?.approved,
        Some(true)
    );
    Ok(())
}

#[test]
fn deferred_confirmation_wire_preserves_boolean_legacy_responses() -> Result<()> {
    assert_eq!(deserialize_approval(serde_json::json!(true))?, Some(true));
    assert_eq!(deserialize_approval(serde_json::Value::Null)?, None);
    assert!(deserialize_approval(serde_json::json!("true")).is_err());
    for approved in [false, true] {
        let value = serde_json::to_value(ToolConfirmationResponse {
            approved: Some(approved),
        })?;
        assert_eq!(value, serde_json::json!({"approved":approved}));
        assert_eq!(
            serde_json::from_value::<ToolConfirmationResponse>(value)?.approved,
            Some(approved)
        );
    }
    assert!(serde_json::from_value::<ToolConfirmationResponse>(serde_json::json!({})).is_err());
    let deferred = serde_json::to_value(ToolConfirmationResponse { approved: None })?;
    assert_eq!(
        serde_json::from_value::<ToolConfirmationResponse>(deferred.clone())?.approved,
        None
    );
    assert_eq!(deferred, serde_json::json!({"approved":null}));
    #[derive(serde::Deserialize)]
    struct OldResponse {
        #[serde(rename = "approved")]
        _approved: bool,
    }
    // Old workers fail closed on the new outcome, rather than treating it as approval.
    assert!(serde_json::from_value::<OldResponse>(deferred).is_err());
    Ok(())
}
