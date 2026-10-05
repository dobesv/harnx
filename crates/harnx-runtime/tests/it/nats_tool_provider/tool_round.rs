//! A tool round journals each NATS call under the sequence of the `ToolCalls`
//! entry that made it, which is where wind-up and replay look for the row.
use super::*;
use futures_util::StreamExt;
use harnx_core::session::SessionLogEntry;
use harnx_core::tool::{NoopToolProgress, ToolCallOrigin, ToolReplay};
use harnx_runtime::config::session::SessionAppendSink;
use harnx_runtime::config::GlobalConfig;
use harnx_runtime::nats_session_log::NatsSessionLog;
use harnx_runtime::tool::{CompletionText, ToolRoundParams, ToolRoundPersistence};
use harnx_runtime::{AgentLoopContext, LoopResult, PendingToolRound, ToolApprovalDecision};
use harnx_toolset_server::invocation_journal::InvocationJournal;

/// A session whose log lives on the broker, and a time tool server registered
/// under the scope its tool rounds discover.
struct RoundHarness {
    _server: common::NatsServerHandle,
    _env: EnvGuard,
    tool_server: tokio::task::JoinHandle<Result<()>>,
    jetstream: async_nats::jetstream::Context,
    instance_id: ServerScope,
    config: GlobalConfig,
    storage_key: String,
}

impl RoundHarness {
    async fn start() -> Result<Option<Self>> {
        let Some(server) = common::spawn_nats_server_with_options(common::SpawnNatsServerOptions {
            auth_token: Some(TOKEN.to_string()),
        })
        .await?
        else {
            return Ok(None);
        };
        let instance_id = ServerScope::new();
        let env = EnvGuard::install(server.url(), TOKEN, &instance_id);
        let server_url = server.url.clone();
        let server_instance = instance_id.clone();
        let tool_server = tokio::spawn(async move {
            serve_over_nats(TimeToolset::new(), server_instance, &server_url, TOKEN).await
        });
        let client = async_nats::ConnectOptions::new()
            .token(TOKEN.to_string())
            .connect(server.url())
            .await?;
        wait_for_registry(&client, &instance_id, "____time").await?;

        let jetstream = async_nats::jetstream::new(client);
        let store = SessionMetadataStore::ensure(&jetstream, 1).await?;
        let metadata = SessionMetadata::new(
            format!("tool-round-{}", uuid::Uuid::new_v4()),
            SessionInitializer::named("metis", Default::default()),
        );
        store.create(&metadata).await?;
        let storage_key = metadata.storage_key();
        let sink = Arc::new(generation::fenced_backend(&jetstream, &storage_key).await?)
            as Arc<dyn SessionAppendSink>;
        let mut session = metadata.base_session();
        session.runtime = Some(Arc::new(sink));
        let config = Arc::new(ConfigLock::new(Config::default()));
        config.write().session = Some(session);
        Ok(Some(Self {
            _server: server,
            _env: env,
            tool_server,
            jetstream,
            instance_id,
            config,
            storage_key,
        }))
    }

    /// A prompt in this session: with no agent named, it saves to the session.
    fn input(&self) -> harnx_runtime::config::input::Input {
        harnx_runtime::config::input::from_str(&self.config, "what time is it", None)
    }

    /// Run one tool round through the runtime, as an agent turn does.
    async fn run_round(
        &self,
        calls: Vec<ToolCall>,
        persistence: ToolRoundPersistence,
    ) -> Result<()> {
        let input = self.input();
        let abort = create_abort_signal();
        harnx_runtime::tool::execute_tool_round_with_persistence(
            ToolRoundParams {
                config: &self.config,
                instance_id: &self.instance_id,
                input: &input,
                completion: CompletionText {
                    output: "checking the time",
                    thought: None,
                },
                abort_signal: &abort,
                working_dir: None,
                nats_hook_provider: None,
                pending_async_context: None,
                tool_loop_guard: None,
            },
            calls,
            persistence,
        )
        .await?;
        Ok(())
    }

    /// An agent turn in this session whose model answers once the tools ran.
    fn loop_context(&self) -> AgentLoopContext {
        AgentLoopContext {
            config: self.config.clone(),
            instance_id: self.instance_id.clone(),
            abort_signal: create_abort_signal(),
            token_budget: None,
            usage_at_start: Default::default(),
            call_fn: Some(Arc::new(|_input, _config, _abort| {
                Box::pin(async {
                    Ok((
                        "It is noon.".to_string(),
                        None,
                        Vec::new(),
                        Default::default(),
                    ))
                })
            })),
            on_tool_round: None,
            on_hitl_approval_required: None,
            on_text_response: None,
            initial_with_embeddings: false,
            initial_resume_count: 0,
            max_resume: Some(0),
            nats_hook_provider: None,
            pending_async_context: None,
            working_dir: None,
            tool_loop_guard: Default::default(),
        }
    }

    fn log(&self) -> NatsSessionLog {
        NatsSessionLog::new_with_replicas(self.jetstream.clone(), &self.storage_key, 1)
    }

    /// Sequence of the `ToolCalls` entry holding `call_id`.
    async fn tool_calls_seq(&self, call_id: &str) -> Result<u64> {
        let entries = self.log().load_events_async().await?;
        entries
            .iter()
            .find_map(|(seq, entry)| match entry {
                SessionLogEntry::ToolCalls { calls, .. }
                    if calls.iter().any(|call| call.id.as_deref() == Some(call_id)) =>
                {
                    Some(*seq)
                }
                _ => None,
            })
            .with_context(|| format!("no ToolCalls entry holds {call_id}"))
    }

    /// The round the journal recorded for the transcript call `call_id`.
    async fn journaled_round(&self, call_id: &str) -> Result<Option<u64>> {
        let rows = InvocationJournal::ensure(&self.jetstream, 1)
            .await?
            .records_for_session(&self.storage_key)
            .await?;
        Ok(rows
            .iter()
            .find(|row| row.request.tool_call_id.as_deref() == Some(call_id))
            .map(|row| row.tool_round))
    }

    /// Requests to create the journal's bucket, which is what opening the
    /// journal sends the server.
    async fn journal_bucket_creates(&self) -> Result<async_nats::Subscriber> {
        let creates = self
            .jetstream
            .client()
            .subscribe(format!(
                "$JS.API.STREAM.CREATE.KV_{}",
                harnx_toolset_server::invocation_journal::BUCKET
            ))
            .await?;
        // The server handles one connection's messages in order, so once it
        // answers a round trip it has registered the subscription.
        self.jetstream.query_account().await?;
        Ok(creates)
    }

    /// How many requests to create the journal's bucket `creates` has seen.
    async fn journal_bucket_opens(&self, mut creates: async_nats::Subscriber) -> Result<usize> {
        // Once the server answers a round trip on the subscription's own
        // connection, it has queued to it every request made before.
        self.jetstream.query_account().await?;
        let mut seen = 0;
        while let Ok(Some(_)) =
            tokio::time::timeout(Duration::from_millis(100), creates.next()).await
        {
            seen += 1;
        }
        Ok(seen)
    }

    async fn stop(self) {
        self.tool_server.abort();
        let _ = self.tool_server.await;
    }
}

fn time_call(id: &str) -> ToolCall {
    ToolCall::new(
        "time_get_current_time".to_string(),
        json!({"timezone": "UTC"}),
        Some(id.to_string()),
        None,
    )
}

/// Dispatch takes the round its caller hands in and reads no transcript to
/// find one. The transcript here holds each call in a `ToolCalls` entry at
/// another sequence, which is what a scan of it would record; a call no round
/// made records zero.
#[tokio::test(flavor = "multi_thread")]
async fn dispatch_journals_the_round_it_is_handed() -> Result<()> {
    let Some(harness) = RoundHarness::start().await? else {
        return Ok(());
    };
    for call_id in ["in-round", "no-round"] {
        harness
            .log()
            .append_event_async(&SessionLogEntry::ToolCalls {
                text: String::new(),
                thought: None,
                calls: vec![time_call(call_id)],
                timestamp: None,
                fence_token: None,
            })
            .await?;
    }
    let config = harness.config.read().clone();
    let provider = NatsToolProvider::discover(
        &config,
        harness.instance_id.clone(),
        NatsInFlightCalls::for_instance(&harness.instance_id),
        None,
    )
    .await?;

    for (call_id, tool_round) in [("in-round", Some(7)), ("no-round", None)] {
        provider
            .call_tool_with_progress(
                "time_get_current_time",
                json!({"timezone": "UTC"}),
                ToolCallOrigin {
                    tool_call_id: Some(call_id),
                    tool_round,
                },
                &create_abort_signal(),
                Arc::new(NoopToolProgress),
            )
            .await
            .map_err(tool_error)?;
    }

    assert_eq!(harness.journaled_round("in-round").await?, Some(7));
    assert_eq!(harness.journaled_round("no-round").await?, Some(0));
    harness.stop().await;
    Ok(())
}

/// Opening the journal asks the server to create its bucket, a request the
/// cluster's meta leader has to answer. A provider keeps the journal it
/// opened, so the calls it journals, the partial result it reads back for a
/// failed call and a replay all share one open.
#[tokio::test(flavor = "multi_thread")]
async fn a_provider_opens_the_journal_once_for_all_its_calls() -> Result<()> {
    let Some(harness) = RoundHarness::start().await? else {
        return Ok(());
    };
    let config = harness.config.read().clone();
    let provider = NatsToolProvider::discover(
        &config,
        harness.instance_id.clone(),
        NatsInFlightCalls::for_instance(&harness.instance_id),
        None,
    )
    .await?;
    let creates = harness.journal_bucket_creates().await?;

    for (call_id, timezone) in [
        ("first", "UTC"),
        ("second", "UTC"),
        ("failing", "Mars/Olympus"),
    ] {
        let outcome = provider
            .call_tool_with_progress(
                "time_get_current_time",
                json!({"timezone": timezone}),
                ToolCallOrigin {
                    tool_call_id: Some(call_id),
                    tool_round: Some(3),
                },
                &create_abort_signal(),
                Arc::new(NoopToolProgress),
            )
            .await;
        if call_id == "failing" {
            assert!(
                matches!(outcome, Err(ToolError::Recoverable(_))),
                "an unknown timezone fails the call recoverably"
            );
        } else {
            outcome.map_err(tool_error)?;
        }
    }
    let first = time_call("first");
    provider
        .replay_tool_call(
            ToolReplay {
                session_id: &harness.storage_key,
                tool_round: 3,
                call: &first,
                worker_id: None,
                fence_token: None,
                authorization: None,
            },
            &create_abort_signal(),
        )
        .await
        .map_err(tool_error)?
        .context("the first call's saved reply")?;

    assert_eq!(
        harness.journal_bucket_opens(creates).await?,
        1,
        "journal bucket opens"
    );
    harness.stop().await;
    Ok(())
}

/// A new round appends its `ToolCalls` entry and dispatches each call under
/// that entry's sequence, not the position the entry was expected to take.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_round_journals_its_calls_under_the_entry_it_appended() -> Result<()> {
    let Some(harness) = RoundHarness::start().await? else {
        return Ok(());
    };
    harness
        .run_round(
            vec![time_call("new-round")],
            ToolRoundPersistence::AppendCalls,
        )
        .await?;

    let appended = harness.tool_calls_seq("new-round").await?;
    assert_eq!(harness.journaled_round("new-round").await?, Some(appended));
    harness.stop().await;
    Ok(())
}

/// An approved round was appended before it waited for approval, and the log
/// moved on since. Resuming it dispatches its calls under the entry they came
/// from, not the log's tail.
#[tokio::test(flavor = "multi_thread")]
async fn an_approved_round_journals_its_calls_under_the_entry_that_made_them() -> Result<()> {
    let Some(harness) = RoundHarness::start().await? else {
        return Ok(());
    };
    let calls = vec![time_call("approved")];
    let input = harness.input();
    harness
        .config
        .write()
        .append_session_tool_calls(&input, "checking the time", None, &calls)?;
    let seq = harness.tool_calls_seq("approved").await?;
    harness
        .log()
        .append_event_async(&SessionLogEntry::HitlApprovalRequested {
            tool_call_id: "approved".to_string(),
            summary: "Check the time".to_string(),
            fence_token: 0,
        })
        .await?;

    let result = harnx_runtime::continue_agent_loop_from_tool_round(
        &harness.loop_context(),
        input,
        PendingToolRound {
            seq,
            output: "checking the time".to_string(),
            thought: None,
            calls,
        },
        vec![ToolApprovalDecision {
            tool_call_id: "approved".to_string(),
            approved: true,
            reason: None,
        }],
        std::collections::BTreeSet::from(["approved".to_string()]),
    )
    .await?;

    assert!(matches!(result, LoopResult::Completed));
    assert_eq!(harness.journaled_round("approved").await?, Some(seq));
    harness.stop().await;
    Ok(())
}
