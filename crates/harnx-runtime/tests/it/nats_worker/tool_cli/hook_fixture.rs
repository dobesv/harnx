//! Tool/hook sidecars and assertion scenarios for CLI operator calls.
use super::*;

struct FixtureTools {
    seen: Arc<std::sync::Mutex<Vec<Value>>>,
    client: async_nats::Client,
    cancelled: Arc<AtomicUsize>,
    dir: PathBuf,
}
#[async_trait::async_trait]
impl Toolset for FixtureTools {
    fn name(&self) -> &str {
        "fixture"
    }
    fn tools(&self) -> Vec<ToolSpec> {
        ["echo", "hidden", "wait", "delegate", "error", "partial", "transport"].into_iter().map(|name| ToolSpec {
            name:name.into(), description: name.into(),
            input_schema: json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"$defs":{"complex":{"oneOf":[{"type":"string"},{"type":"null"}]}}}),
            cancellation_guarantee: Default::default(), idempotent_hint:false, read_only_hint:false, timeout_secs:Some(0), meta:None,
        }).collect()
    }
    async fn invoke(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
    ) -> std::result::Result<Value, ToolInvokeError> {
        unreachable!()
    }
    async fn invoke_with_context(
        &self,
        invocation: ToolInvocation,
    ) -> std::result::Result<Value, ToolInvokeError> {
        if !invocation.args.get("text").is_some_and(Value::is_string) {
            return Err(ToolInvokeError::Recoverable("text must be a string".into()));
        }
        self.seen.lock().unwrap().push(invocation.args.clone());
        match invocation.tool.as_str() {
            "wait" => {
                invocation.cancel.cancelled().await;
                self.cancelled.fetch_add(1, Ordering::SeqCst);
                Err(ToolInvokeError::Recoverable("cancelled fixture".into()))
            }
            "error" => Ok(
                json!({"isError":true,"content":[{"type":"text","text":"fixture error"}],"structuredContent":{"detail":7},"_meta":{"kept":true}}),
            ),
            "partial" => {
                Ok(json!({"resultType":"partial","data":[1,2],"reason":"fixture partial"}))
            }
            "transport" => Err(ToolInvokeError::Fatal("fixture protocol failure".into())),
            "delegate" => self.delegate(invocation).await,
            _ => Ok(
                json!({"content":[{"type":"text","text":"complete mixed output"},{"type":"image","data":"AA==","mimeType":"image/png"},{"type":"resource_link","uri":"https://example.com/tool","name":"result"}],"structuredContent":{"args":invocation.args,"caller":invocation.context.invoking_session_id,"session_ref":invocation.context.invoking_session},"_meta":{"kept":true},"isError":false}),
            ),
        }
    }
}

impl FixtureTools {
    async fn delegate(
        &self,
        invocation: ToolInvocation,
    ) -> std::result::Result<Value, ToolInvokeError> {
        let nested = NatsSession::new(
            NatsSessionConfig {
                cluster: "local".into(),
                initializer: SessionInitializer::named("limited", Default::default()),
                session_id: Some(format!("cli-nested-{}", invocation.context.call_id)),
                activation_route: harnx_runtime::SessionActivationRoute::ClusterShared,
            },
            self.client.clone(),
            async_nats::jetstream::new(self.client.clone()),
            create_abort_signal(),
        )
        .await
        .map_err(|e| ToolInvokeError::Recoverable(e.to_string()))?;
        let lineage = invocation
            .context
            .run_context
            .as_ref()
            .expect("operator CALL lineage");
        let parent = serde_json::from_value(lineage.snapshot.clone()).unwrap();
        let admitted_at =
            chrono::DateTime::from_timestamp_millis(lineage.started_at_ms.try_into().unwrap())
                .unwrap();
        let nested = nested.with_inherited_admission(
            parent,
            invocation.context.call_id,
            admitted_at,
            harnx_runtime::nats_session_metadata::InvocationEdgeKind::Delegation,
            None,
        );
        nested
            .enqueue_text("nested tool request")
            .await
            .map_err(|e| ToolInvokeError::Recoverable(e.to_string()))?;
        let log = NatsSessionLog::new(
            async_nats::jetstream::new(self.client.clone()),
            nested.storage_key(),
        );
        tokio::time::timeout(CI_SAFE_TIMEOUT, async {
            loop {
                if log.load_events_async().await?.iter().any(|(_, entry)| {
                    matches!(entry, SessionLogEntry::HitlApprovalRequested { .. })
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .map_err(|e| ToolInvokeError::Recoverable(e.to_string()))?
        .map_err(|e| ToolInvokeError::Recoverable(e.to_string()))?;
        let events = log
            .load_events_async()
            .await
            .map_err(|e| ToolInvokeError::Recoverable(e.to_string()))?;
        assert!(!events
            .iter()
            .any(|(_, entry)| matches!(entry, SessionLogEntry::HitlApprovalDecision { .. })));
        std::fs::write(self.dir.join("nested-deferred"), nested.storage_key()).unwrap();
        Ok(json!({"partial":true,"nested":nested.storage_key(),"reason":"nested Ask deferred"}))
    }
}

struct Policy;
#[async_trait::async_trait]
impl harnx_hookset::Hook for Policy {
    fn name(&self) -> &str {
        "cli-policy"
    }
    fn hooks(&self) -> Vec<harnx_hookset::HookSpec> {
        vec![harnx_hookset::HookSpec {
            event: "PreToolUse".into(),
            matcher: Some("^fixture_".into()),
            priority: 0,
            timeout_secs: None,
            fail_policy: harnx_hookset::FailPolicy::Closed,
        }]
    }
    async fn handle_hook(
        &self,
        payload: harnx_core::hooks::HookPayload,
    ) -> harnx_hooks::HookOutcome {
        use harnx_hooks::{HookEvent, HookOutcome, HookResult, HookResultControl};
        match payload.hook_event {
            HookEvent::PreToolUse { tool_input, .. } if tool_input["text"] == "deny" => {
                HookOutcome {
                    control: HookResultControl::Block {
                        reason: "CLI policy denied".into(),
                    },
                    result: Default::default(),
                }
            }
            HookEvent::PreToolUse { tool_input, .. } if tool_input["text"] == "mutate" => {
                HookOutcome {
                    control: HookResultControl::Ask {
                        reason: Some("root Ask after mutation".into()),
                    },
                    result: HookResult {
                        mutated_tool_input: Some(
                            json!({"text":"mutated with spaces and \"quotes\""}),
                        ),
                        ..Default::default()
                    },
                }
            }
            _ => HookOutcome {
                control: HookResultControl::Ask {
                    reason: Some("explicit Ask".into()),
                },
                result: Default::default(),
            },
        }
    }
}

pub(super) struct CliHookFixture<'a> {
    h: &'a Harness,
    tools: Arc<FixtureTools>,
    shutdown: CancellationToken,
    _tool_server: AbortOnDropHandle<Result<()>>,
    _hook_server: AbortOnDropHandle<Result<()>>,
}

impl<'a> CliHookFixture<'a> {
    pub(super) async fn start(h: &'a Harness) -> Result<Self> {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let cancelled = Arc::new(AtomicUsize::new(0));
        let shutdown = CancellationToken::new();
        let tools = Arc::new(FixtureTools {
            seen: seen.clone(),
            client: h.client.clone(),
            cancelled: cancelled.clone(),
            dir: h.dir.path().into(),
        });
        let tool_server =
            AbortOnDropHandle::new(tokio::spawn(harnx_toolset_server::serve_with_shutdown(
                tools.clone(),
                h.scope.clone(),
                harnx_nats_common::connect::NatsConnection {
                    client: h.client.clone(),
                    replicas: 1,
                },
                harnx_toolset_server::ServeLifecycle::new(shutdown.clone(), None),
            )));
        let hook_server =
            AbortOnDropHandle::new(tokio::spawn(harnx_hookset_server::serve_with_shutdown(
                Arc::new(Policy),
                h.scope.clone(),
                harnx_nats_common::connect::NatsConnection {
                    client: h.client.clone(),
                    replicas: 1,
                },
                harnx_hookset_server::ServeLifecycle::new(shutdown.clone(), None),
            )));
        let fixture = Self {
            h,
            tools,
            shutdown,
            _tool_server: tool_server,
            _hook_server: hook_server,
        };
        fixture.wait_ready().await?;
        Ok(fixture)
    }

    async fn wait_ready(&self) -> Result<()> {
        let h = self.h;
        poll_until(async || {
            let cfg = h.config.read().clone();
            let provider = harnx_runtime::nats_tool_provider::NatsToolProvider::discover_strict(
                &cfg,
                h.scope.clone(),
                Default::default(),
                None,
            )
            .await?;
            let hooks = harnx_runtime::nats_hook_provider::NatsHookProvider::discover_with_client(
                h.client.clone(),
                h.scope.clone(),
            )
            .await?;
            Ok(provider
                .declarations()
                .iter()
                .filter(|tool| tool.name.starts_with("fixture_"))
                .count()
                == 7
                && !hooks.hooks().is_empty())
        })
        .await?;
        Ok(())
    }

    pub(super) async fn assert_inventory(&self) -> Result<()> {
        let h = self.h;
        let seen = &self.tools.seen;
        let list = h
            .json(&["list", "tools", "fixture_*", "--json"], true)
            .await?;
        assert_eq!(list.as_array().unwrap().len(), 7);
        let group = h
            .json(&["list", "tools", "inspection", "--json"], true)
            .await?;
        assert_eq!(group.as_array().unwrap().len(), 2);
        let scoped_group = h
            .json(
                &[
                    "--agent",
                    "limited@local",
                    "list",
                    "tools",
                    "inspection",
                    "--json",
                ],
                true,
            )
            .await?;
        assert_eq!(scoped_group.as_array().unwrap().len(), 1);
        assert_eq!(scoped_group[0]["name"], "fixture_echo");
        let limited = h
            .json(
                &["--agent", "limited@local", "list", "tools", "--json"],
                true,
            )
            .await?;
        assert_eq!(limited.as_array().unwrap().len(), 2);
        let hidden = h
            .json(
                &[
                    "--agent",
                    "limited@local",
                    "call",
                    "tool",
                    "fixture_hidden",
                    r#"{"text":"must not run"}"#,
                    "--json",
                ],
                false,
            )
            .await?;
        assert!(hidden["error"].as_str().unwrap().contains("not available"));
        assert!(seen.lock().unwrap().is_empty());
        Ok(())
    }

    pub(super) async fn assert_transformed_results(&self) -> Result<()> {
        let h = self.h;
        let seen = &self.tools.seen;
        let result = h
            .json(
                &[
                    "call",
                    "tool",
                    "fixture_echo",
                    r#"{"text":"mutate"}"#,
                    "--json",
                ],
                true,
            )
            .await?;
        assert_eq!(
            result["structuredContent"]["args"]["text"],
            "mutated with spaces and \"quotes\""
        );
        assert_eq!(result["content"][1]["data"], "AA==");
        assert_eq!(result["content"][2]["uri"], "https://example.com/tool");
        assert_eq!(result["_meta"]["kept"], true);
        assert!(result["structuredContent"]["caller"].as_str().is_some());
        assert_eq!(seen.lock().unwrap().len(), 1);
        let named = h
            .json(
                &[
                    "--agent",
                    "limited@local",
                    "call",
                    "tool",
                    "fixture_echo",
                    r#"{"text":"mutate"}"#,
                    "--json",
                ],
                true,
            )
            .await?;
        assert_eq!(
            named["structuredContent"]["args"]["text"],
            "mutated with spaces and \"quotes\""
        );
        assert_eq!(
            named["structuredContent"]["session_ref"]["agent"],
            "limited"
        );
        Ok(())
    }

    pub(super) async fn assert_deny_validation(&self) -> Result<()> {
        let h = self.h;
        let seen = &self.tools.seen;
        let seen_before = seen.lock().unwrap().len();
        let denied = h
            .json(
                &[
                    "call",
                    "tool",
                    "fixture_echo",
                    r#"{"text":"deny"}"#,
                    "--json",
                ],
                false,
            )
            .await?;
        assert_eq!(denied["blocked_by_hook"], true);
        assert_eq!(
            seen.lock().unwrap().len(),
            seen_before,
            "Deny dispatched tool"
        );
        let invalid = h
            .json(
                &["call", "tool", "fixture_echo", r#"{"text":7}"#, "--json"],
                false,
            )
            .await?;
        assert!(invalid.to_string().contains("text must be a string"));
        assert_eq!(seen.lock().unwrap().len(), seen_before);
        Ok(())
    }

    pub(super) async fn assert_error_partial_transport(&self) -> Result<()> {
        let h = self.h;
        let error = h
            .json(
                &[
                    "call",
                    "tool",
                    "fixture_error",
                    r#"{"text":"error"}"#,
                    "--json",
                ],
                false,
            )
            .await?;
        assert_eq!(error["isError"], true);
        assert_eq!(error["structuredContent"]["detail"], 7);
        assert_eq!(error["_meta"]["kept"], true);
        let partial = h
            .json(
                &[
                    "call",
                    "tool",
                    "fixture_partial",
                    r#"{"text":"partial"}"#,
                    "--json",
                ],
                false,
            )
            .await?;
        assert_eq!(partial["data"], json!([1, 2]));
        let transport = h
            .json(
                &[
                    "call",
                    "tool",
                    "fixture_transport",
                    r#"{"text":"error"}"#,
                    "--json",
                ],
                false,
            )
            .await?;
        assert!(transport.to_string().contains("fixture protocol failure"));
        Ok(())
    }

    pub(super) async fn assert_timeout_cleanup(&self) -> Result<()> {
        let h = self.h;
        let cancelled = &self.tools.cancelled;
        let timed = h
            .run(&[
                "call",
                "tool",
                "fixture_wait",
                r#"{"text":"wait"}"#,
                "--json",
                "--timeout-secs",
                "5",
            ])
            .await?;
        assert!(!timed.status.success());
        let _: Value = serde_json::from_slice(&timed.stdout)?;
        assert!(String::from_utf8_lossy(&timed.stderr).contains("timed out"));
        wait_until(CI_SAFE_TIMEOUT, || cancelled.load(Ordering::SeqCst) == 1).await?;
        let named_timeout = h
            .run(&[
                "--agent",
                "waiter@local",
                "call",
                "tool",
                "fixture_wait",
                r#"{"text":"named wait"}"#,
                "--json",
                "--timeout-secs",
                "5",
            ])
            .await?;
        assert!(!named_timeout.status.success());
        let _: Value = serde_json::from_slice(&named_timeout.stdout)?;
        assert!(String::from_utf8_lossy(&named_timeout.stderr).contains("timed out"));
        wait_until(CI_SAFE_TIMEOUT, || cancelled.load(Ordering::SeqCst) == 2).await?;
        Ok(())
    }

    pub(super) async fn assert_nested_consent(&self) -> Result<()> {
        let h = self.h;
        let seen = &self.tools.seen;
        let before = seen.lock().unwrap().len();
        let nested = h
            .json(
                &[
                    "call",
                    "tool",
                    "fixture_delegate",
                    r#"{"text":"delegate"}"#,
                    "--json",
                ],
                false,
            )
            .await?;
        assert_eq!(nested["partial"], true);
        assert!(h.dir.path().join("nested-deferred").is_file());
        assert_eq!(
            seen.lock().unwrap().len(),
            before + 1,
            "nested Ask inherited root consent"
        );
        assert_eq!(
            h.calls.load(Ordering::SeqCst),
            1,
            "root unexpectedly inferred"
        );
        let before = seen.lock().unwrap().len();
        let named_nested = h
            .json(
                &[
                    "--agent",
                    "limited@local",
                    "call",
                    "tool",
                    "fixture_delegate",
                    r#"{"text":"delegate"}"#,
                    "--json",
                ],
                false,
            )
            .await?;
        assert_eq!(named_nested["partial"], true);
        assert_eq!(
            seen.lock().unwrap().len(),
            before + 1,
            "named root consent leaked into nested call"
        );
        assert_eq!(h.calls.load(Ordering::SeqCst), 2);
        Ok(())
    }

    pub(super) fn stop(&self) {
        self.shutdown.cancel();
    }
}
