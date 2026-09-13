use super::*;
use std::time::Duration;

struct Server {
    child: std::process::Child,
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
            serde_json::to_vec(&ToolConfirmationResponse { approved: true })?.into(),
        )
        .await?;
    let response = tokio::time::timeout(Duration::from_secs(5), decision)
        .await?
        .context("confirmation was cancelled")??;
    assert!(serde_json::from_slice::<ToolConfirmationResponse>(&response.payload)?.approved);
    Ok(())
}
