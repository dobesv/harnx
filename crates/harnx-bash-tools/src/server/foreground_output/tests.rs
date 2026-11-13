use super::*;

#[tokio::test]
async fn cancelled_collection_keeps_completed_stdout() {
    let (release, blocked) = tokio::sync::oneshot::channel();
    let stdout = tokio::spawn(async { Ok(b"stdout".to_vec()) });
    let stderr = tokio::spawn(async move {
        blocked.await.unwrap();
        Ok(b"stderr".to_vec())
    });
    let mut output = ForegroundOutput::new(stdout, stderr);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), output.finish())
            .await
            .is_err()
    );
    assert!(
        output.stdout.is_some(),
        "stdout should already have been joined"
    );
    release.send(()).unwrap();
    output.finish().await;
    output.finish().await;
    assert_eq!(
        output.into_strings().unwrap(),
        ("stdout".into(), "stderr".into())
    );
}

#[tokio::test]
async fn collection_keeps_pipe_errors_without_repolling_join_handle() {
    let stdout = tokio::spawn(async { Err(std::io::Error::other("reader failed")) });
    let stderr = tokio::spawn(async { Ok(Vec::new()) });
    let mut output = ForegroundOutput::new(stdout, stderr);
    output.finish().await;
    output.finish().await;
    assert!(output
        .into_strings()
        .unwrap_err()
        .to_string()
        .contains("reader failed"));
}

#[tokio::test]
async fn bounded_collection_keeps_completed_stream_when_other_never_finishes() {
    for blocked_stdout in [true, false] {
        let blocked = tokio::spawn(std::future::pending::<std::io::Result<Vec<u8>>>());
        let abort = blocked.abort_handle();
        let ready = tokio::spawn(async { Ok(b"collected output".to_vec()) });
        let (stdout, stderr) = if blocked_stdout {
            (blocked, ready)
        } else {
            (ready, blocked)
        };
        let output = ForegroundOutput::new(stdout, stderr);
        let result = tokio::time::timeout(Duration::from_secs(2), output.finish_bounded())
            .await
            .expect("error drain must not wait for EOF");
        abort.abort();
        let expected = if blocked_stdout {
            (String::new(), "collected output".into())
        } else {
            ("collected output".into(), String::new())
        };
        assert_eq!(result, expected);
    }
}

#[tokio::test]
async fn bounded_collection_returns_when_neither_stream_finishes() {
    let stdout = tokio::spawn(std::future::pending::<std::io::Result<Vec<u8>>>());
    let stderr = tokio::spawn(std::future::pending::<std::io::Result<Vec<u8>>>());
    let abort_stdout = stdout.abort_handle();
    let abort_stderr = stderr.abort_handle();
    let output = ForegroundOutput::new(stdout, stderr);
    let result = tokio::time::timeout(Duration::from_secs(2), output.finish_bounded())
        .await
        .expect("error drain must not wait for either EOF");
    abort_stdout.abort();
    abort_stderr.abort();
    assert_eq!(result, (String::new(), String::new()));
}

#[tokio::test]
async fn bounded_collection_keeps_other_stream_after_reader_error() {
    let stdout = tokio::spawn(async { Err(std::io::Error::other("reader failed")) });
    let stderr = tokio::spawn(async { Ok(b"collected stderr".to_vec()) });
    let output = ForegroundOutput::new(stdout, stderr);
    assert_eq!(
        output.finish_bounded().await,
        (String::new(), "collected stderr".into())
    );
}
