use super::*;
use crate::{
    ExtFunctionResult, MontyObject, ScriptHostContext, ScriptPythonFunctions, ScriptPythonHandler,
    ScriptRequest, MAX_SCRIPT_NESTING,
};

#[derive(Clone, Copy)]
enum Mode {
    Bash,
    Python,
    Spawned,
    Background,
}

struct Nested {
    remaining: usize,
    mode: Mode,
    entered: Option<Arc<Barrier>>,
}

impl Nested {
    async fn next(&self, cancel: CancellationToken) -> ScriptRun {
        if let Some(barrier) = &self.entered {
            barrier.wait().await;
        }
        // Force an async boundary between the interpreter and nested admission.
        tokio::task::yield_now().await;
        chain(self.remaining - 1, self.mode, None, cancel).await
    }
}

#[async_trait]
impl ScriptCommand for Nested {
    fn name(&self) -> &str {
        "nested"
    }

    async fn run(
        &self,
        _args: &[String],
        _stdin: Option<&[u8]>,
        ctx: ScriptCommandContext<'_>,
    ) -> Result<ScriptCommandOutput, String> {
        let result = if matches!(self.mode, Mode::Spawned) {
            let remaining = self.remaining;
            let cancel = ctx.cancel;
            tokio::spawn(async move {
                ctx.control
                    .scope(chain(remaining - 1, Mode::Spawned, None, cancel))
                    .await
            })
            .await
            .unwrap()
        } else {
            self.next(ctx.cancel).await
        };
        if result.status != ScriptStatus::Ok {
            return Err(result.error.unwrap_or(result.stderr));
        }
        Ok(ScriptCommandOutput {
            stdout: result.stdout,
            ..Default::default()
        })
    }
}

#[async_trait]
impl ScriptPythonHandler for Nested {
    async fn call(
        &self,
        name: String,
        _args: Vec<MontyObject>,
        _kwargs: Vec<(MontyObject, MontyObject)>,
        ctx: ScriptHostContext,
    ) -> ExtFunctionResult {
        assert_eq!(name, "nested");
        let result = self.next(ctx.cancel).await;
        if result.status != ScriptStatus::Ok {
            return ExtFunctionResult::Error(crate::MontyException::new(
                crate::ExcType::RuntimeError,
                result.error,
            ));
        }
        ExtFunctionResult::Return(MontyObject::string(result.stdout))
    }
}

async fn chain(
    remaining: usize,
    mode: Mode,
    entered: Option<Arc<Barrier>>,
    cancel: CancellationToken,
) -> ScriptRun {
    let nested = Arc::new(Nested {
        remaining,
        mode,
        entered,
    });
    let script = if remaining == 0 {
        "printf leaf"
    } else if matches!(mode, Mode::Python) {
        "python3 -c \"print(nested(), end='')\""
    } else if matches!(mode, Mode::Background) {
        "nested & wait"
    } else {
        "nested"
    };
    BashKitBackend
        .run(ScriptRequest {
            script,
            limits: ScriptLimits {
                timeout_secs: 10,
                ..Default::default()
            },
            commands: vec![nested.clone()],
            python_functions: Some(ScriptPythonFunctions {
                names: vec!["nested".into()],
                handler: nested,
            }),
            fs_mounts: vec![],
            cancel,
        })
        .await
}

#[tokio::test]
async fn nesting_eight_concurrent_roots_past_eight_levels_completes() {
    let entered = Arc::new(Barrier::new(8));
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let entered = entered.clone();
        tasks.spawn(chain(
            11,
            Mode::Bash,
            Some(entered),
            CancellationToken::new(),
        ));
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(result) = tasks.join_next().await {
            let result = result.unwrap();
            assert_eq!(result.status, ScriptStatus::Ok, "{result:?}");
            assert_eq!(result.stdout, "leaf");
        }
    })
    .await
    .expect("nested interpreter admission deadlocked");
}

#[tokio::test]
async fn nesting_python_host_awaits_and_explicit_spawn_scope_keep_depth() {
    for mode in [Mode::Python, Mode::Spawned, Mode::Background] {
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            chain(11, mode, None, CancellationToken::new()),
        )
        .await
        .expect("host nesting deadlocked");
        assert_eq!(result.status, ScriptStatus::Ok, "{result:?}");
        assert_eq!(result.stdout, "leaf");
    }
}

#[tokio::test]
async fn nesting_max_depth_returns_error_without_waiting_and_restores_context() {
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        chain(
            MAX_SCRIPT_NESTING,
            Mode::Bash,
            None,
            CancellationToken::new(),
        ),
    )
    .await
    .expect("max-depth admission hung");
    assert_eq!(result.status, ScriptStatus::Error, "{result:?}");
    assert!(
        result
            .error
            .as_deref()
            .unwrap()
            .contains("script nesting depth limit exceeded (maximum 16 runs)"),
        "{result:?}"
    );
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        chain(
            MAX_SCRIPT_NESTING - 1,
            Mode::Bash,
            None,
            CancellationToken::new(),
        ),
    )
    .await
    .expect("depth context leaked after error");
    assert_eq!(result.status, ScriptStatus::Ok, "{result:?}");
    assert_eq!(result.stdout, "leaf");
}
