use super::*;
use crate::operator_tools::BashToolSelection;

impl LiveOperatorFixture {
    /// #2630 Task 4: the three worker-path selection rows (named inferred,
    /// named explicit, TUI session) against a real fixture tool server. The
    /// seeded agent's U contains fixture_echo/fixture_wait/fixture_delegate
    /// but not fixture_hidden, which stays discoverable but unconfigured.
    pub(super) async fn assert_bash_matrix(&self) -> Result<()> {
        let Self {
            tools,
            hooks_seen,
            model_calls,
            ..
        } = self;
        let FixtureTools { seen, .. } = tools.as_ref();
        let model_before = model_calls.load(Ordering::SeqCst);
        let hooks_before = hooks_seen.load(Ordering::SeqCst);
        let seen_before = seen.lock().unwrap().len();

        self.assert_bash_session_output().await?;
        self.assert_bash_inferred_admission(seen_before).await?;
        self.assert_bash_explicit_admission().await;
        self.assert_bash_shell_and_blocking().await;
        self.assert_bash_in_process_admission().await?;

        assert!(hooks_seen.load(Ordering::SeqCst) > hooks_before);
        assert_eq!(model_calls.load(Ordering::SeqCst), model_before);
        assert_eq!(
            model_calls.load(Ordering::SeqCst),
            0,
            "operator bash commands must not create inference turns"
        );
        Ok(())
    }

    async fn run_bash(&self, script: &str, selection: BashToolSelection) -> OperatorToolReply {
        let request = self.request.clone();
        let client = self.client.clone();
        let script = script.to_string();
        call(
            &client,
            &request,
            OperatorToolCommand::Bash { script, selection },
        )
        .await
    }

    async fn assert_bash_session_output(&self) -> Result<()> {
        // TUI session row: admission is U ∪ B, so the configured fixture_echo
        // runs (through the policy's Ask, via operator inner consent).
        let reply = self
            .run_bash(
                r#"result=$(fixture_echo --text "from session"); echo "got:$result""#,
                BashToolSelection::Session,
            )
            .await;
        assert!(reply.error.is_none(), "{reply:?}");
        let payload: Value = serde_json::from_str(&reply.output)?;
        assert_eq!(payload["status"], "ok");
        assert_eq!(payload["trace"][0]["name"], "fixture_echo");
        // The captured tool output is the fixture's structured reply rendered
        // by the existing inner-call semantics; the script saw it verbatim.
        assert!(
            payload["stdout"]
                .as_str()
                .unwrap_or_default()
                .starts_with("got:"),
            "{reply:?}"
        );
        assert!(
            payload["stdout"]
                .as_str()
                .unwrap_or_default()
                .contains("metis"),
            "{reply:?}"
        );

        Ok(())
    }

    async fn assert_bash_inferred_admission(&self, seen_before: usize) -> Result<()> {
        let seen = &self.tools.seen;
        // Named inferred row: claims are the literal names only; admission
        // stays U ∪ B. fixture_hidden is discovered but not configured, so
        // inference naming it admits nothing and never widens.
        let denied = self
            .run_bash(
                "fixture_hidden --text nope",
                BashToolSelection::Inferred {
                    names: vec!["fixture_hidden".into()],
                },
            )
            .await;
        assert!(
            denied.error.is_some(),
            "inferred name must not widen: {denied:?}"
        );
        assert_eq!(seen.lock().unwrap().len(), seen_before + 1);
        // ...while the configured fixture_echo still runs under U ∪ B even
        // when the inference names nothing.
        let admitted = self
            .run_bash(
                "fixture_echo --text inferred",
                BashToolSelection::Inferred { names: Vec::new() },
            )
            .await;
        assert!(admitted.error.is_none(), "{admitted:?}");

        Ok(())
    }

    async fn assert_bash_explicit_admission(&self) {
        // Named explicit row: E adds fixture_hidden on top of U ∪ B.
        let added = self
            .run_bash(
                "fixture_hidden --text explicit",
                BashToolSelection::Explicit {
                    selectors: vec!["fixture_hidden".into()],
                },
            )
            .await;
        assert!(added.error.is_none(), "{added:?}");
        // Explicit presence with empty selectors disables inference: U ∪ B
        // only, so fixture_hidden goes back to unavailable.
        let no_inference = self
            .run_bash(
                "fixture_hidden --text still no",
                BashToolSelection::Explicit {
                    selectors: Vec::new(),
                },
            )
            .await;
        assert!(
            no_inference.error.is_some(),
            "explicit empty must not infer: {no_inference:?}"
        );
    }

    async fn assert_bash_shell_and_blocking(&self) {
        // Pure shell with an empty catalog keeps the synthetic outer bash
        // active and dispatches nothing.
        let shell = self
            .run_bash(
                "echo hello",
                BashToolSelection::Inferred { names: Vec::new() },
            )
            .await;
        assert!(shell.error.is_none(), "{shell:?}");
        assert!(shell.output.contains("hello"), "{shell:?}");
        // Hook Block still blocks inner script calls.
        let blocked = self
            .run_bash("fixture_echo --text deny", BashToolSelection::Session)
            .await;
        assert!(
            blocked.error.is_some(),
            "hook Block must fail the script: {blocked:?}"
        );
        assert!(blocked.output.contains("policy denied"), "{blocked:?}");
    }

    /// #2655 finding B regression: a selected in-process tool is callable
    /// from an operator Bash script even though no tool server advertises
    /// it — the same catalog ordinary agent Bash gets for the same U. An
    /// unselected in-process tool stays out of the catalog. The fixture
    /// server only advertises `fixture_*` names, so dispatch here proves
    /// the in-process providers answered.
    async fn assert_bash_in_process_admission(&self) -> Result<()> {
        for (script, selection, row) in [
            (
                r#"result=$(harnx_read_session_meta); echo "meta:$result""#,
                BashToolSelection::Session,
                "session",
            ),
            (
                r#"result=$(harnx_read_session_meta); echo "meta:$result""#,
                BashToolSelection::Inferred { names: Vec::new() },
                "named inferred",
            ),
        ] {
            let reply = self.run_bash(script, selection).await;
            assert!(reply.error.is_none(), "{row} row: {reply:?}");
            let payload: Value = serde_json::from_str(&reply.output)?;
            assert_eq!(payload["status"], "ok", "{row} row: {reply:?}");
            assert_eq!(
                payload["trace"][0]["name"],
                crate::session_meta_tool::READ_TOOL_NAME,
                "{row} row: {reply:?}"
            );
            assert_eq!(payload["trace"][0]["status"], "ok", "{row} row: {reply:?}");
            let stdout = payload["stdout"].as_str().unwrap_or_default();
            let view: Value = serde_json::from_str(stdout.strip_prefix("meta:").unwrap_or(""))?;
            assert_eq!(
                view["session_id"], "active-operator",
                "{row} row: {reply:?}"
            );
        }
        // harnx_agent_session_history_read is an in-process built-in that U
        // does not select: it must stay out of the script catalog, exactly
        // like the unconfigured fixture_hidden.
        let unselected = self
            .run_bash(
                "harnx_agent_session_history_read",
                BashToolSelection::Session,
            )
            .await;
        assert!(
            unselected.error.is_some(),
            "unselected in-process tool must not be admitted: {unselected:?}"
        );

        Ok(())
    }

    /// #2630 Task 4: cancelling an operator Bash command reaches the inner
    /// tool through the worker's abort plumbing, and the cancelled tool is
    /// observed releasing. No replay happens: the next command still works.
    pub(super) async fn assert_bash_cancellation(&self) -> Result<()> {
        let Self { client, .. } = self;
        let FixtureTools {
            started, cancelled, ..
        } = self.tools.as_ref();
        let abort = crate::utils::create_abort_signal();
        let mut waiting = self.request.clone();
        waiting.command = OperatorToolCommand::Bash {
            script: "fixture_wait --text waiting".into(),
            selection: BashToolSelection::Session,
        };
        let waiting_task = tokio::spawn({
            let client = client.clone();
            let abort = abort.clone();
            async move {
                request_command(&client, "cluster.local.operator_tools", &waiting, &abort).await
            }
        });
        tokio::time::timeout(Duration::from_secs(30), started.notified()).await?;
        abort.set_ctrlc();
        assert!(waiting_task
            .await?
            .unwrap_err()
            .to_string()
            .contains("cancelled"));
        tokio::time::timeout(Duration::from_secs(30), cancelled.notified()).await?;
        // The claim released cleanly: a follow-up command executes normally.
        let reply = call(
            client,
            &self.request.clone(),
            OperatorToolCommand::Bash {
                script: "echo recovered".into(),
                selection: BashToolSelection::Session,
            },
        )
        .await;
        assert!(reply.error.is_none(), "{reply:?}");
        assert!(reply.output.contains("recovered"), "{reply:?}");
        Ok(())
    }
}

/// Run one `.set` through the real frontend flow: the same metadata sink
/// `NatsSession::from_global_config` attaches to the active session, and
/// `Config::update`'s parse → CAS persist → in-memory apply sequence. This is
/// how a TUI `.set bash_tools` reaches the session metadata bucket.
fn set_via_dot_command(session: &NatsSession, command: &str) -> Result<()> {
    let backend = crate::nats_worker::NatsSessionLogBackend::new(
        session.jetstream().clone(),
        session.storage_key().to_owned(),
        session.attachment_replicas(),
    )
    .with_metadata_store(Some(session.metadata_store().clone()));
    let sink: Arc<dyn crate::config::session::SessionAppendSink> = Arc::new(backend);
    let frontend = crate::config::GlobalConfig::default();
    {
        let mut config = frontend.write();
        let mut active = crate::config::session::new(&config, "frontend-session", None)?;
        active.runtime = Some(Arc::new(sink));
        config.session = Some(active);
    }
    crate::config::Config::update(&frontend, command)
}

async fn run_session_bash(fixture: &LiveOperatorFixture, script: &str) -> OperatorToolReply {
    let mut request = fixture.request.clone();
    request.command = OperatorToolCommand::Bash {
        script: script.into(),
        selection: BashToolSelection::Session,
    };
    call(&fixture.client, &request, request.command.clone()).await
}

/// #2630 Task 4 regression: effective session bash_tools reach worker Bash
/// admission. `run_session_tool_command` transports only the session's
/// `use_tools`; the worker reconstructs effective B from the session metadata
/// override applied over the installed agent (`install_session_metadata_agent`
/// → `apply_session_overrides` → `extract_agent().bash_tools()`). This test
/// drives the real frontend `.set` flow into the metadata bucket, then asserts
/// admission through the real request → worker subscription, covering the
/// frontmatter fallback, an overriding `.set`, and the explicit clear.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_worker_session_bash_tools_overrides_reach_bash_admission() -> Result<()> {
    let _lock = env_lock().await;
    let Some((url, mut nats, _store)) = spawn_test_nats().await else {
        return Ok(());
    };
    let seeded = seed_remote_config(&url);
    let _env = subagent_test_env(&url, &seeded);
    // hermes: U = fixture_echo only; the agent file's B names fixture_hidden,
    // which is deliberately outside U so B-only admission is distinguishable.
    std::fs::write(
        seeded.config_dir().join("agents/hermes.md"),
        "---\nmodel: test:test-model\nuse_tools: [fixture_echo]\nbash_tools: [fixture_hidden]\n---\nhermes agent\n",
    )?;
    let scope = ServerScope::new();
    let _scope = TestEnvGuard::new("HARNX_SERVER_SCOPE", scope.as_str());
    let client = async_nats::connect(&url).await?;
    let fixture = LiveOperatorFixture::start(
        seeded.parent_config.clone(),
        client,
        scope,
        SessionIdentity {
            id: "session-bash-tools",
            agent: "hermes",
        },
    )
    .await?;
    fixture.wait_ready().await?;
    // The production frontend request carries the session's effective use_tools;
    // bash_tools is not part of the protocol.
    let mut request = fixture.request.clone();
    request.use_tools = Some(vec!["fixture_echo".into()]);
    let fixture = LiveOperatorFixture { request, ..fixture };

    assert_session_bash_overrides(&fixture).await?;

    assert_eq!(
        fixture.model_calls.load(Ordering::SeqCst),
        0,
        "operator bash commands must not create inference turns"
    );
    fixture.close().await?;
    nats.kill()?;
    nats.wait()?;
    Ok(())
}

/// #2655 regression (CodeRabbit finding A, comment 4515991614): a request
/// that disables tool use against a worker with tool use enabled must
/// admit no Bash inner tools — none from effective use_tools (U), none
/// from bash_tools (B), none from explicit selectors (E) — in any
/// selection mode, while the synthetic `bash` still runs as pure shell.
/// Direct `.call tool` already gates its selectors the same way; this pins
/// the Bash worker path to those semantics, and covers worker-level
/// disablement with an enabled request.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_worker_bash_tool_use_disabled_admits_no_inner_tools() -> Result<()> {
    let _lock = env_lock().await;
    let Some((url, mut nats, _store)) = spawn_test_nats().await else {
        return Ok(());
    };
    let seeded = seed_remote_config(&url);
    let _env = subagent_test_env(&url, &seeded);
    // hermes: U = fixture_echo, B = fixture_hidden, so U and B are
    // independently observable admission sources; E adds fixture_hidden
    // explicitly. The worker config keeps tool_use enabled throughout.
    std::fs::write(
        seeded.config_dir().join("agents/hermes.md"),
        "---\nmodel: test:test-model\nuse_tools: [fixture_echo]\nbash_tools: [fixture_hidden]\n---\nhermes agent\n",
    )?;
    let scope = ServerScope::new();
    let _scope = TestEnvGuard::new("HARNX_SERVER_SCOPE", scope.as_str());
    let client = async_nats::connect(&url).await?;
    let fixture = LiveOperatorFixture::start(
        seeded.parent_config.clone(),
        client,
        scope,
        SessionIdentity {
            id: "session-bash-tool-use",
            agent: "hermes",
        },
    )
    .await?;
    fixture.wait_ready().await?;
    // Production request shape: effective session use_tools in the request.
    let mut request = fixture.request.clone();
    request.use_tools = Some(vec!["fixture_echo".into()]);
    let fixture = LiveOperatorFixture { request, ..fixture };

    // Enabled baseline: session mode admits fixture_echo through U.
    let enabled = run_session_bash(&fixture, "fixture_echo --text enabled").await;
    assert!(
        enabled.error.is_none(),
        "enabled baseline must admit U: {enabled:?}"
    );

    assert_request_disabled_modes_admit_nothing(&fixture).await?;
    assert_request_disabled_call_and_shell_parity(&fixture).await?;
    assert_worker_disablement_is_not_sticky(&fixture).await?;

    assert_eq!(
        fixture.model_calls.load(Ordering::SeqCst),
        0,
        "operator bash commands must not create inference turns"
    );
    fixture.close().await?;
    nats.kill()?;
    nats.wait()?;
    Ok(())
}

/// Request-level disablement, selection rows: U via session mode, U via
/// inferred names, B via session mode, and E via explicit selectors must all
/// stay out of the catalog, with no inner tool dispatched.
async fn assert_request_disabled_modes_admit_nothing(fixture: &LiveOperatorFixture) -> Result<()> {
    let FixtureTools { seen, .. } = fixture.tools.as_ref();
    let seen_before = seen.lock().unwrap().len();
    let mut disabled = fixture.request.clone();
    disabled.tool_use = false;
    for (script, selection, source) in [
        (
            "fixture_echo --text session",
            BashToolSelection::Session,
            "U via session mode",
        ),
        (
            "fixture_echo --text inferred",
            BashToolSelection::Inferred {
                names: vec!["fixture_echo".into()],
            },
            "U via inferred names",
        ),
        (
            "fixture_hidden --text session",
            BashToolSelection::Session,
            "B via session mode",
        ),
        (
            "fixture_hidden --text explicit",
            BashToolSelection::Explicit {
                selectors: vec!["fixture_hidden".into()],
            },
            "E via explicit selectors",
        ),
    ] {
        let reply = call(
            &fixture.client,
            &disabled,
            OperatorToolCommand::Bash {
                script: script.into(),
                selection,
            },
        )
        .await;
        assert!(
            reply.error.is_some(),
            "{source} must not admit with tool use disabled: {reply:?}"
        );
    }
    assert_eq!(
        seen.lock().unwrap().len(),
        seen_before,
        "disabled tool use must not dispatch any inner tool"
    );
    Ok(())
}

/// Request-level disablement, parity rows: a direct `.call tool` on the same
/// disabled request stays refused, and the synthetic `bash` still runs as
/// pure shell without dispatching an inner tool.
async fn assert_request_disabled_call_and_shell_parity(
    fixture: &LiveOperatorFixture,
) -> Result<()> {
    let FixtureTools { seen, .. } = fixture.tools.as_ref();
    let seen_before = seen.lock().unwrap().len();
    let mut disabled = fixture.request.clone();
    disabled.tool_use = false;
    // Direct `.call tool` parity: the same request stays refused.
    let direct = call(
        &fixture.client,
        &disabled,
        OperatorToolCommand::Call {
            name: "fixture_echo".into(),
            args_json: "{}".into(),
        },
    )
    .await;
    assert!(
        direct.error.is_some(),
        "direct call must stay refused with tool use disabled: {direct:?}"
    );

    // Pure shell still runs: only the synthetic `bash` remains.
    let shell = call(
        &fixture.client,
        &disabled,
        OperatorToolCommand::Bash {
            script: "echo pure-shell".into(),
            selection: BashToolSelection::Session,
        },
    )
    .await;
    assert!(
        shell.error.is_none(),
        "pure shell must still run: {shell:?}"
    );
    assert!(shell.output.contains("pure-shell"), "{shell:?}");
    assert_eq!(
        seen.lock().unwrap().len(),
        seen_before,
        "pure shell must not dispatch any inner tool"
    );
    Ok(())
}

/// Worker-level disablement with an enabled request: same refusal, and the
/// gate is per request, not sticky — re-enabling restores admission.
async fn assert_worker_disablement_is_not_sticky(fixture: &LiveOperatorFixture) -> Result<()> {
    let FixtureTools { seen, .. } = fixture.tools.as_ref();
    let seen_before = seen.lock().unwrap().len();
    fixture.global.write().tool_use = false;
    let worker_disabled = run_session_bash(fixture, "fixture_echo --text worker-disabled").await;
    assert!(
        worker_disabled.error.is_some(),
        "worker-level disablement must not admit U: {worker_disabled:?}"
    );
    assert_eq!(
        seen.lock().unwrap().len(),
        seen_before,
        "worker-level disablement must not dispatch any inner tool"
    );

    // Re-enabling restores admission.
    fixture.global.write().tool_use = true;
    let restored = run_session_bash(fixture, "fixture_echo --text restored").await;
    assert!(
        restored.error.is_none(),
        "re-enabled tool use must admit again: {restored:?}"
    );
    Ok(())
}

async fn assert_session_bash_overrides(fixture: &LiveOperatorFixture) -> Result<()> {
    // No override yet: the agent file's bash_tools are the session's effective B.
    let baseline = run_session_bash(fixture, "fixture_hidden --text via-frontmatter").await;
    assert!(
        baseline.error.is_none(),
        "agent-file bash_tools must reach worker admission: {baseline:?}"
    );

    // `.set bash_tools fixture_quiet` replaces B (session override, not
    // additive). fixture_quiet is deliberately outside U so its admission
    // proves the override itself reached the worker.
    set_via_dot_command(&fixture.session, "bash_tools fixture_quiet")?;
    let replaced = run_session_bash(fixture, "fixture_hidden --text replaced").await;
    assert!(
        replaced.error.is_some(),
        "session bash_tools override must replace the agent file's list: {replaced:?}"
    );
    let via_override = run_session_bash(fixture, "fixture_quiet --text via-override").await;
    assert!(
        via_override.error.is_none(),
        "overridden session bash_tools must admit: {via_override:?}"
    );

    // `.set bash_tools null`: explicit clear, distinct from "no override" —
    // the agent file's list must not resurface.
    set_via_dot_command(&fixture.session, "bash_tools null")?;
    let cleared = run_session_bash(fixture, "fixture_quiet --text cleared").await;
    assert!(
        cleared.error.is_some(),
        "explicit clear must empty effective session bash_tools: {cleared:?}"
    );
    let frontmatter = run_session_bash(fixture, "fixture_hidden --text resurfaced").await;
    assert!(
        frontmatter.error.is_some(),
        "explicit clear must not fall back to the agent file's bash_tools: {frontmatter:?}"
    );
    let use_intact = run_session_bash(fixture, "fixture_echo --text use-intact").await;
    assert!(
        use_intact.error.is_none(),
        "use_tools must stay admitted after clearing bash_tools: {use_intact:?}"
    );

    Ok(())
}
