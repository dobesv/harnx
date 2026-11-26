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
            prelude: None,
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
                        prelude: None,
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

async fn prelude_run(prelude: &str, script: &str) -> ScriptRun {
    let host = Arc::new(Host::default());
    BashKitBackend
        .run(ScriptRequest {
            script,
            limits: ScriptLimits::default(),
            commands: vec![],
            python_functions: Some(ScriptPythonFunctions {
                names: vec!["answer".into()],
                handler: host,
                prelude: Some(prelude.into()),
            }),
            fs_mounts: vec![],
            cancel: CancellationToken::new(),
        })
        .await
}

const THREE_LINE_PRELUDE: &str = "a = 1\nb = 2\nc = 3";
const RAISES_ON_LINE_FOUR: &str = "x = 1\ny = 2\nz = 3\nraise ValueError(\"boom\")";

#[tokio::test]
async fn a_prelude_defines_names_the_script_sees() {
    let result = prelude_run("x = 41", "python3 -c 'print(x + 1)'").await;
    assert_eq!(result.status, ScriptStatus::Ok, "{result:?}");
    assert_eq!(result.stdout, "42\n");
}

#[tokio::test]
async fn an_error_reports_the_scripts_own_line() {
    let scripts = [
        format!("python3 - <<'EOF'\n{RAISES_ON_LINE_FOUR}\nEOF"),
        format!("python3 -c '{RAISES_ON_LINE_FOUR}'"),
        format!("cat > /tmp/x.py <<'EOF'\n{RAISES_ON_LINE_FOUR}\nEOF\npython3 /tmp/x.py"),
    ];
    for script in scripts {
        let result = prelude_run(THREE_LINE_PRELUDE, &script).await;
        assert_eq!(result.status, ScriptStatus::Error, "{result:?}");
        assert!(result.stderr.contains("line 4"), "{result:?}");
        assert!(!result.stderr.contains("line 8"), "{result:?}");
        assert!(result.stderr.contains("ValueError: boom"), "{result:?}");
    }
}

#[tokio::test]
async fn a_syntax_error_reports_the_scripts_own_line() {
    let script = "python3 - <<'EOF'\nx = 1\ny = 2\nz = 3\nif x ==\nEOF";
    let result = prelude_run(THREE_LINE_PRELUDE, script).await;
    assert_eq!(result.status, ScriptStatus::Error, "{result:?}");
    assert!(result.stderr.contains("line 4"), "{result:?}");
    assert!(!result.stderr.contains("line 7"), "{result:?}");
}

#[tokio::test]
async fn a_frame_in_the_prelude_is_named() {
    let prelude = "def _harnx_boom():\n    raise ValueError('inside')";
    let script = "python3 - <<'EOF'\nx = 1\n_harnx_boom()\nEOF";
    let result = prelude_run(prelude, script).await;
    assert_eq!(result.status, ScriptStatus::Error, "{result:?}");
    assert!(
        result.stderr.contains("File \"<harnx tools>\", line 2"),
        "{result:?}"
    );
    assert!(result.stderr.contains("line 2, in <module>"), "{result:?}");
}

#[tokio::test]
async fn a_successful_scripts_stderr_is_untouched() {
    let result = prelude_run(
        THREE_LINE_PRELUDE,
        "python3 --version; python3 -c 'print(c)'",
    )
    .await;
    assert_eq!(result.status, ScriptStatus::Ok, "{result:?}");
    assert_eq!(result.stdout, "Python 3.12.0 (monty)\n3\n");
    assert_eq!(result.stderr, "", "{result:?}");
}

#[tokio::test]
async fn empty_and_newline_terminated_preludes_keep_the_scripts_own_line() {
    for prelude in ["", "a = 1\n", "a = 1\nb = 2\n\n"] {
        let script = format!("python3 - <<'EOF'\n{RAISES_ON_LINE_FOUR}\nEOF");
        let result = prelude_run(prelude, &script).await;
        assert_eq!(
            result.status,
            ScriptStatus::Error,
            "{prelude:?}: {result:?}"
        );
        assert!(result.stderr.contains("line 4"), "{prelude:?}: {result:?}");
    }
}

const EXEC_SCRIPT: &str = "x = 1\ny = 2\nz = 3\nexec(\"a = 1\\nraise ValueError('inner')\")";

#[tokio::test]
async fn an_exec_frame_under_a_heredoc_keeps_its_own_line() {
    let scripts = [
        format!("python3 - <<'EOF'\n{EXEC_SCRIPT}\nEOF"),
        format!("cat > /tmp/x.py <<'EOF'\n{EXEC_SCRIPT}\nEOF\npython3 /tmp/x.py"),
    ];
    for script in scripts {
        let result = prelude_run(THREE_LINE_PRELUDE, &script).await;
        assert_eq!(result.status, ScriptStatus::Error, "{result:?}");
        assert!(
            result.stderr.contains("File \"<string>\", line 2"),
            "{result:?}"
        );
        assert!(!result.stderr.contains("<harnx tools>"), "{result:?}");
        assert!(
            result.stderr.contains(", line 4, in <module>"),
            "{result:?}"
        );
    }
}

const FUTURE_SCRIPT: &str =
    "python3 - <<'EOF'\nfrom __future__ import annotations\nprint('ran')\nEOF";
const FUTURE_HINT: &str =
    "python: a script that can call tools cannot use \"from __future__\" imports; remove that line";

#[tokio::test]
async fn a_future_import_fails_with_a_hint_when_a_prelude_is_set() {
    let result = prelude_run(THREE_LINE_PRELUDE, FUTURE_SCRIPT).await;
    assert_eq!(result.status, ScriptStatus::Error, "{result:?}");
    assert_eq!(result.stdout, "", "{result:?}");
    assert!(
        result.stderr.contains("File \"<stdin>\", line 1"),
        "{result:?}"
    );
    assert!(result.stderr.contains(FUTURE_HINT), "{result:?}");
}

#[tokio::test]
async fn a_future_import_runs_without_a_prelude() {
    let result = host_run(FUTURE_SCRIPT, Arc::new(Host::default())).await;
    assert_eq!(result.status, ScriptStatus::Ok, "{result:?}");
    assert_eq!(result.stdout, "ran\n");
}

#[tokio::test]
async fn another_failure_gets_no_future_hint() {
    let result = prelude_run(THREE_LINE_PRELUDE, "python3 -c 'raise ValueError(1)'").await;
    assert_eq!(result.status, ScriptStatus::Error, "{result:?}");
    assert!(!result.stderr.contains(FUTURE_HINT), "{result:?}");
}

/// Records the tool name of every `_harnx_call` it receives.
#[derive(Default)]
struct NameRecorder(Mutex<Vec<String>>);

#[async_trait]
impl ScriptPythonHandler for NameRecorder {
    async fn call(
        &self,
        name: String,
        args: Vec<MontyObject>,
        _kwargs: Vec<(MontyObject, MontyObject)>,
        _ctx: ScriptHostContext,
    ) -> ExtFunctionResult {
        assert_eq!(name, crate::PYTHON_CALL_FUNCTION);
        let tool = args.first().and_then(|arg| arg.as_ref().as_str());
        self.0.lock().unwrap().push(
            tool.expect("the first argument is the tool name")
                .to_owned(),
        );
        ExtFunctionResult::Return(MontyObject::none())
    }
}

async fn run_with_tools(names: &[String], python: &str) -> (ScriptRun, Vec<String>) {
    let recorder = Arc::new(NameRecorder::default());
    let result = BashKitBackend
        .run(ScriptRequest {
            script: &format!("python3 - <<'PY'\n{python}\nPY"),
            limits: ScriptLimits::default(),
            commands: vec![],
            python_functions: Some(ScriptPythonFunctions {
                names: vec![crate::PYTHON_CALL_FUNCTION.into()],
                handler: recorder.clone(),
                prelude: Some(crate::tools_prelude(names)),
            }),
            fs_mounts: vec![],
            cancel: CancellationToken::new(),
        })
        .await;
    let seen = recorder.0.lock().unwrap().clone();
    (result, seen)
}

#[tokio::test]
async fn hostile_tool_names_reach_the_handler_through_the_real_interpreter() {
    let names: Vec<String> = [
        "has\"quote",
        "back\\slash",
        "new\nline",
        "h\u{e9}llo \u{2713}",
        "__class__",
        "class",
        "tools",
    ]
    .iter()
    .map(|name| name.to_string())
    .collect();
    let list = serde_json::to_string(&names).unwrap();
    let (result, seen) = run_with_tools(
        &names,
        &format!("for n in {list}:\n    getattr(tools, n)(1)\nprint('done')"),
    )
    .await;
    assert_eq!(result.status, ScriptStatus::Ok, "{result:?}");
    assert_eq!(result.stdout, "done\n", "{result:?}");
    assert_eq!(seen, names);
    let (result, seen) = run_with_tools(&names, "print('untouched')").await;
    assert_eq!(result.status, ScriptStatus::Ok, "{result:?}");
    assert_eq!(result.stdout, "untouched\n");
    assert!(seen.is_empty());
}

#[tokio::test]
async fn an_unknown_tool_names_the_tools_object() {
    let (result, _) = run_with_tools(
        &["known".into()],
        "try:\n    tools.nope\nexcept AttributeError as e:\n    print(e)",
    )
    .await;
    assert_eq!(result.stdout, "'tools' object has no attribute 'nope'\n");
}
