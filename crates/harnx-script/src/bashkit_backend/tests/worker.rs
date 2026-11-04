use super::*;

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
