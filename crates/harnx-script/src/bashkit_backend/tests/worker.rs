use super::*;
use std::future::Future;

struct PanicOnDrop;

impl Drop for PanicOnDrop {
    fn drop(&mut self) {
        panic!("command cleanup panicked at deadline");
    }
}

struct PanicDuringCleanup;

#[async_trait]
impl ScriptCommand for PanicDuringCleanup {
    fn name(&self) -> &str {
        "panic_during_cleanup"
    }

    async fn run(
        &self,
        _args: &[String],
        _stdin: Option<&[u8]>,
        _ctx: ScriptCommandContext<'_>,
    ) -> Result<ScriptCommandOutput, String> {
        // Cancellation drops this pending future. Panic then, rather than race
        // a timer against the host deadline.
        let _guard = PanicOnDrop;
        std::future::pending().await
    }
}

#[tokio::test]
async fn timeout_preserves_worker_panic_diagnostic() {
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        BashKitBackend.run(crate::ScriptRequest {
            script: "panic_during_cleanup",
            limits: ScriptLimits {
                timeout_secs: 1,
                ..Default::default()
            },
            commands: vec![Arc::new(PanicDuringCleanup)],
            fs_mounts: vec![],
            cancel: CancellationToken::new(),
        }),
    )
    .await
    .expect("panicking worker must be joined after timeout");
    assert_timeout(&result);
    let diagnostic = result.error.as_deref().expect("worker panic diagnostic");
    assert!(diagnostic.contains("script worker failed:"), "{result:?}");
    assert!(
        diagnostic.contains("command cleanup panicked at deadline"),
        "{result:?}"
    );
    assert_eq!(result.stderr.trim_end(), diagnostic);
    assert_ne!(result.error, result.limit_hit);
}

#[tokio::test]
async fn completed_run_is_not_reclassified_when_parent_resumes_after_deadline() {
    let mut run = Box::pin(BashKitBackend.run(crate::ScriptRequest {
        script: "echo completed",
        limits: ScriptLimits {
            timeout_secs: 1,
            ..Default::default()
        },
        commands: vec![],
        fs_mounts: vec![],
        cancel: CancellationToken::new(),
    }));
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(run.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    // Don't poll the parent while the interpreter completes. Returning its
    // admission permit proves it has left the blocking closure, without sleeps.
    tokio::time::timeout(Duration::from_millis(900), async {
        while crate::bashkit_backend::worker::ADMISSION.available_permits()
            != crate::bashkit_backend::worker::MAX_SCRIPT_WORKERS
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let result = run.await;
    assert_eq!(result.status, ScriptStatus::Ok, "{result:?}");
    assert_eq!(result.stdout, "completed\n");
    assert_eq!(result.exit_code, 0);
    assert!(result.limit_hit.is_none());
    assert!(result.error.is_none());
}

#[tokio::test]
async fn admission_wait_observes_cancel_and_deadline_without_starting_script() {
    use crate::bashkit_backend::worker::{ADMISSION, MAX_SCRIPT_WORKERS};
    let _all = ADMISSION
        .clone()
        .acquire_many_owned(MAX_SCRIPT_WORKERS as u32)
        .await
        .unwrap();
    for timed_out in [false, true] {
        let token = CancellationToken::new();
        let mut run = Box::pin(BashKitBackend.run(crate::ScriptRequest {
            script: "echo must-not-start",
            limits: ScriptLimits {
                timeout_secs: 1,
                ..Default::default()
            },
            commands: vec![],
            fs_mounts: vec![],
            cancel: token.clone(),
        }));
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(run.as_mut().poll(cx)))
                .await
                .is_pending()
        );
        if !timed_out {
            token.cancel();
        }
        let result = tokio::time::timeout(Duration::from_secs(2), run)
            .await
            .unwrap();
        assert!(result.stdout.is_empty());
        if timed_out {
            assert_timeout(&result);
            assert_eq!(
                result.error.as_deref(),
                Some("execution timeout exceeded (1s)")
            );
        } else {
            assert_eq!(result.status, ScriptStatus::Cancelled);
        }
        assert_eq!(ADMISSION.available_permits(), 0);
    }
}

#[test]
fn cancelled_queued_closures_retain_admission_until_blocking_pool_drains() {
    use crate::bashkit_backend::worker::{ADMISSION, MAX_SCRIPT_WORKERS};
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    let (release, wait) = std::sync::mpsc::channel::<()>();
    let (started, ready) = std::sync::mpsc::channel();
    let blocker = runtime.spawn_blocking(move || {
        started.send(()).unwrap();
        let _ = wait.recv();
    });
    ready.recv_timeout(Duration::from_secs(2)).unwrap();
    runtime.block_on(async {
        for remaining in (0..MAX_SCRIPT_WORKERS).rev() {
            let token = CancellationToken::new();
            let mut run = Box::pin(BashKitBackend.run(crate::ScriptRequest {
                script: "echo must-not-start",
                limits: ScriptLimits::default(),
                commands: vec![],
                fs_mounts: vec![],
                cancel: token.clone(),
            }));
            assert!(
                std::future::poll_fn(|cx| std::task::Poll::Ready(run.as_mut().poll(cx)))
                    .await
                    .is_pending()
            );
            token.cancel();
            assert_eq!(run.await.status, ScriptStatus::Cancelled);
            assert_eq!(ADMISSION.available_permits(), remaining);
        }
    });
    drop(release);
    runtime.block_on(blocker).unwrap();
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(2), async {
            while ADMISSION.available_permits() != MAX_SCRIPT_WORKERS {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    });
}
