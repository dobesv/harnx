use harnx_acp::session_prompt_with_abort_for_test;
use anyhow::Result;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{oneshot, Notify};

struct FakePromptClient {
    started: Arc<Notify>,
    release_prompt: Arc<Notify>,
    cancelled_sessions: Arc<Mutex<Vec<String>>>,
}

impl FakePromptClient {
    fn new() -> Self {
        Self {
            started: Arc::new(Notify::new()),
            release_prompt: Arc::new(Notify::new()),
            cancelled_sessions: Arc::new(Mutex::new(Vec::new())),
        }
    }

    async fn session_prompt(&self, _session_id: Option<&str>, _message: &str) -> Result<String> {
        self.started.notify_waiters();
        self.release_prompt.notified().await;
        Ok("completed".to_string())
    }

    async fn session_cancel(&self, session_id: &str) -> Result<()> {
        self.cancelled_sessions
            .lock()
            .expect("cancelled sessions mutex should lock")
            .push(session_id.to_string());
        Ok(())
    }

    async fn wait_until_prompt_started(&self) {
        self.started.notified().await;
    }

    fn cancelled_sessions(&self) -> Vec<String> {
        self.cancelled_sessions
            .lock()
            .expect("cancelled sessions mutex should lock")
            .clone()
    }
}

#[tokio::test(flavor = "current_thread")]
async fn test_call_tool_session_prompt_ctrl_c_cancels_session() {
    let client = Arc::new(FakePromptClient::new());
    let prompt_client = Arc::clone(&client);
    let cancel_client = Arc::clone(&client);
    let session_id = "session-1".to_string();

    let (abort_tx, abort_rx) = oneshot::channel::<()>();
    let call_task = tokio::spawn(async move {
        session_prompt_with_abort_for_test(
            move |session_id, message| {
                let client = Arc::clone(&prompt_client);
                async move { client.session_prompt(session_id.as_deref(), &message).await }
            },
            move |session_id| {
                let client = Arc::clone(&cancel_client);
                async move { client.session_cancel(&session_id).await }
            },
            session_id,
            "please hang".to_string(),
            async move {
                let _ = abort_rx.await;
            },
        )
        .await
    });

    client.wait_until_prompt_started().await;
    abort_tx.send(()).expect("trigger ACP abort");

    let result = call_task.await.expect("call task should join cleanly");
    let err = result.expect_err("abort should fail ACP session prompt");
    assert!(err.to_string().contains("aborted by user"));
    assert_eq!(client.cancelled_sessions(), vec!["session-1".to_string()]);
    client.release_prompt.notify_waiters();
}

#[tokio::test(flavor = "current_thread")]
async fn test_call_tool_session_prompt_ctrl_c_cancel_timeout_is_best_effort() {
    let session_id = "session-timeout".to_string();

    let (abort_tx, abort_rx) = oneshot::channel::<()>();
    let call_task = tokio::spawn(async move {
        session_prompt_with_abort_for_test(
            |_session_id, _message| async move {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok("completed".to_string())
            },
            |_session_id| async move {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok(())
            },
            session_id,
            "please hang".to_string(),
            async move {
                let _ = abort_rx.await;
            },
        )
        .await
    });

    abort_tx.send(()).expect("trigger ACP abort");

    let result = tokio::time::timeout(Duration::from_secs(6), call_task)
        .await
        .expect("abort path should not block indefinitely")
        .expect("call task should join cleanly");
    let err = result.expect_err("abort should fail ACP session prompt");
    assert!(err.to_string().contains("aborted by user"));
}
