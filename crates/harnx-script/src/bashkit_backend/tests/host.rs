use super::*;
use crate::{
    ExcType, ExtFunctionResult, MontyException, MontyObject, ScriptHostContext,
    ScriptPythonFunctions, ScriptPythonHandler, ScriptRequest,
};

#[derive(Default)]
struct Host {
    calls: AtomicUsize,
    outcome: Mutex<Option<HostOutcome>>,
}

#[derive(Debug, PartialEq)]
enum HostOutcome {
    InputRequired,
}

impl Host {
    async fn answer(&self) -> i64 {
        tokio::task::yield_now().await;
        self.calls.fetch_add(1, Ordering::SeqCst);
        42
    }

    fn functions(self: &Arc<Self>) -> Option<ScriptPythonFunctions> {
        Some(ScriptPythonFunctions {
            names: vec![
                "answer".into(),
                "host_stop".into(),
                "fail".into(),
                "callable".into(),
            ],
            handler: self.clone(),
        })
    }
}

#[async_trait]
impl ScriptCommand for Host {
    fn name(&self) -> &str {
        "answer"
    }

    async fn run(
        &self,
        _args: &[String],
        _stdin: Option<&[u8]>,
        ctx: ScriptCommandContext<'_>,
    ) -> Result<ScriptCommandOutput, String> {
        assert!(!ctx.cancel.is_cancelled());
        Ok(ScriptCommandOutput {
            stdout: format!("{}\n", self.answer().await),
            ..Default::default()
        })
    }
}

#[async_trait]
impl ScriptPythonHandler for Host {
    async fn call(
        &self,
        name: String,
        args: Vec<MontyObject>,
        kwargs: Vec<(MontyObject, MontyObject)>,
        ctx: ScriptHostContext,
    ) -> ExtFunctionResult {
        assert!(!ctx.cancel.is_cancelled());
        match name.as_str() {
            "answer" => {
                assert_eq!(args, vec![MontyObject::int(7)]);
                assert_eq!(
                    kwargs,
                    vec![(MontyObject::string("id"), MontyObject::string("step"))]
                );
                ExtFunctionResult::Return(MontyObject::dict(vec![
                    (
                        MontyObject::string("answer"),
                        MontyObject::int(self.answer().await),
                    ),
                    (MontyObject::string("ok"), MontyObject::bool(true)),
                ]))
            }
            "host_stop" => {
                *self.outcome.lock().unwrap() = Some(HostOutcome::InputRequired);
                ctx.control.stop();
                // Even returning an exception must not resume a Python except block.
                ExtFunctionResult::Error(MontyException::new(
                    ExcType::RuntimeError,
                    Some("stopped".into()),
                ))
            }
            "fail" => ExtFunctionResult::Error(MontyException::new(
                ExcType::ValueError,
                Some("bad argument".into()),
            )),
            "callable" => ExtFunctionResult::Return(MontyObject::function("not_registered", None)),
            _ => panic!("unregistered function dispatched: {name}"),
        }
    }
}

async fn host_run(script: &str, host: Arc<Host>) -> ScriptRun {
    BashKitBackend
        .run(ScriptRequest {
            script,
            limits: ScriptLimits::default(),
            commands: vec![host.clone()],
            python_functions: host.functions(),
            fs_mounts: vec![],
            cancel: CancellationToken::new(),
        })
        .await
}

#[tokio::test]
async fn python_registered_function_and_bash_share_async_host_operation() {
    let host = Arc::new(Host::default());
    let result = host_run(
        "answer; python3 -c \"r = answer(7, id='step'); print(r['answer']); print(r['ok'])\"",
        host.clone(),
    )
    .await;
    assert_eq!(result.status, ScriptStatus::Ok, "{result:?}");
    assert_eq!(result.stdout, "42\n42\nTrue\n");
    assert_eq!(host.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn python_unregistered_names_and_returned_callables_cannot_dispatch() {
    let host = Arc::new(Host::default());
    for code in ["not_registered()", "callable()()"] {
        let result = host_run(&format!("python3 -c '{code}'"), host.clone()).await;
        assert_eq!(result.status, ScriptStatus::Error, "{result:?}");
        assert!(result.stderr.contains("NameError"), "{result:?}");
    }
    assert_eq!(host.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn python_host_exceptions_remain_catchable_script_errors() {
    let result = host_run(
        "python3 - <<'PY'\ntry:\n fail()\nexcept ValueError:\n print('caught')\nPY",
        Arc::new(Host::default()),
    )
    .await;
    assert_eq!(result.status, ScriptStatus::Ok, "{result:?}");
    assert_eq!(result.stdout, "caught\n");
}

#[tokio::test]
async fn python_host_stop_is_not_catchable_and_preserves_typed_host_state() {
    let host = Arc::new(Host::default());
    let result = tokio::time::timeout(Duration::from_secs(3), host_run(
        "set +e; python3 - <<'PY'\ntry:\n host_stop()\nexcept Exception:\n print('caught')\nprint('after')\nPY\necho after-shell", host.clone()
    )).await.expect("host stop hung");
    assert_eq!(result.status, ScriptStatus::Stopped, "{result:?}");
    assert_eq!(result.exit_code, 130);
    assert!(result.stdout.is_empty(), "{result:?}");
    assert!(result.error.is_none(), "{result:?}");
    assert_eq!(
        *host.outcome.lock().unwrap(),
        Some(HostOutcome::InputRequired)
    );
    assert_eq!(run("exit 130").await.status, ScriptStatus::Error);
    assert_eq!(run("echo fresh").await.status, ScriptStatus::Ok);
}

struct StopCommand(Arc<ProbeCommand>);

#[async_trait]
impl ScriptCommand for StopCommand {
    fn name(&self) -> &str {
        "host_stop"
    }

    async fn run(
        &self,
        _args: &[String],
        _stdin: Option<&[u8]>,
        ctx: ScriptCommandContext<'_>,
    ) -> Result<ScriptCommandOutput, String> {
        self.0.started.acquire().await.unwrap().forget();
        ctx.control.stop();
        Ok(ScriptCommandOutput::default())
    }
}

#[tokio::test]
async fn bash_host_stop_ignores_errexit_and_drains_background_jobs() {
    for script in [
        "probe & host_stop; echo after",
        "probe & host_stop & wait; echo after",
    ] {
        let probe = ProbeCommand::new(Behavior::Pending);
        let cancel = CancellationToken::new();
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            BashKitBackend.run(ScriptRequest {
                script: &format!("set +e; {script}"),
                limits: ScriptLimits {
                    max_stderr_bytes: 0,
                    ..Default::default()
                },
                commands: vec![Arc::new(StopCommand(probe.clone())), probe.clone()],
                python_functions: None,
                fs_mounts: vec![],
                cancel: cancel.clone(),
            }),
        )
        .await
        .expect("host stop hung");
        assert_eq!(result.status, ScriptStatus::Stopped, "{result:?}");
        assert!(result.stdout.is_empty(), "{result:?}");
        assert_eq!(probe.active.load(Ordering::SeqCst), 0);
        assert!(probe
            .tokens
            .lock()
            .unwrap()
            .iter()
            .all(CancellationToken::is_cancelled));
        assert!(!cancel.is_cancelled());
    }
}

#[async_trait]
impl ScriptPythonHandler for ProbeCommand {
    async fn call(
        &self,
        _name: String,
        _args: Vec<MontyObject>,
        _kwargs: Vec<(MontyObject, MontyObject)>,
        ctx: ScriptHostContext,
    ) -> ExtFunctionResult {
        self.active.fetch_add(1, Ordering::SeqCst);
        let _guard = ActiveGuard(self.active.clone());
        self.tokens.lock().unwrap().push(ctx.cancel);
        self.started.add_permits(1);
        std::future::pending().await
    }
}

#[tokio::test]
async fn python_host_wait_observes_inclusive_deadline_and_cancellation() {
    for timed_out in [false, true] {
        let probe = ProbeCommand::new(Behavior::Pending);
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let handler = probe.clone();
        let task = tokio::spawn(async move {
            BashKitBackend
                .run(ScriptRequest {
                    script: "echo before; python3 -c 'host_wait()'; echo after",
                    limits: ScriptLimits {
                        timeout_secs: 1,
                        ..Default::default()
                    },
                    commands: vec![],
                    python_functions: Some(ScriptPythonFunctions {
                        names: vec!["host_wait".into()],
                        handler,
                    }),
                    fs_mounts: vec![],
                    cancel: task_cancel,
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), probe.started.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
        if !timed_out {
            cancel.cancel();
        }
        let result = tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .expect("Python host wait ignored stop")
            .unwrap();
        if timed_out {
            assert_timeout(&result);
        } else {
            assert_eq!(result.status, ScriptStatus::Cancelled, "{result:?}");
        }
        assert_eq!(result.stdout, "before\n");
        probe.assert_single_stopped();
    }
}
