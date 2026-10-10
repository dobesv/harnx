use super::*;
use harnx_runtime::client::{
    ChatCompletionsData, ChatCompletionsOutput, Client, ExtraConfig, Model, RequestPatches,
    SseHandler, TestStateGuard,
};
use harnx_runtime::test_utils::{MockClient, MockTurnBuilder};
use std::sync::Mutex;

struct GatedSummarizer {
    inner: MockClient,
    entered: Notify,
    release: Notify,
    calls: AtomicUsize,
}

impl GatedSummarizer {
    fn new() -> Self {
        Self {
            inner: MockClient::builder()
                .default_turn(
                    MockTurnBuilder::new()
                        .add_text_chunk("older work completed")
                        .build(),
                )
                .build(),
            entered: Notify::new(),
            release: Notify::new(),
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl Client for GatedSummarizer {
    fn extra_config(&self) -> Option<&ExtraConfig> {
        self.inner.extra_config()
    }
    fn patches_config(&self) -> Option<&RequestPatches> {
        self.inner.patches_config()
    }
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn model(&self) -> &Model {
        self.inner.model()
    }
    fn model_mut(&mut self) -> &mut Model {
        self.inner.model_mut()
    }

    async fn chat_completions_inner(
        &self,
        client: &reqwest::Client,
        data: ChatCompletionsData,
    ) -> Result<ChatCompletionsOutput> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.entered.notify_one();
            self.release.notified().await;
        }
        self.inner.chat_completions_inner(client, data).await
    }

    async fn chat_completions_streaming_inner(
        &self,
        _client: &reqwest::Client,
        _handler: &mut SseHandler,
        _data: ChatCompletionsData,
    ) -> Result<()> {
        anyhow::bail!("compaction fixture expects non-streaming requests")
    }
}

#[derive(Default)]
struct TurnRequests {
    prompts: Vec<String>,
    messages: Vec<Vec<(MessageRole, String)>>,
}

fn capture_turns(requests: Arc<Mutex<TurnRequests>>) -> harnx_runtime::agent_loop::AgentCallFn {
    Arc::new(move |input, config, _abort| {
        let mut input = input.clone();
        let client = MockClient::builder().build();
        let data = harnx_runtime::config::input::prepare_completion_data(
            &mut input,
            config,
            client.model(),
            false,
            &client,
        );
        let requests = requests.clone();
        Box::pin(async move {
            let messages = data?
                .messages
                .into_iter()
                .map(|m| (m.role, m.content.to_text()))
                .collect();
            let mut requests = requests.lock().unwrap();
            requests.prompts.push(input.raw.0.clone());
            requests.messages.push(messages);
            Ok((
                format!("completed: {}", input.raw.0),
                None,
                vec![],
                CompletionTokenUsage::default(),
            ))
        })
    })
}

async fn seed_history(js: &async_nats::jetstream::Context, id: &str) -> Result<NatsSessionLog> {
    let store = SessionMetadataStore::ensure(js, 1).await?;
    store
        .create(&SessionMetadata::new(
            id,
            SessionInitializer::inline(
                "",
                Default::default(),
                SessionOverrides {
                    compress_threshold: Some(1),
                    ..Default::default()
                },
            ),
        ))
        .await?;
    let log = NatsSessionLog::new_with_replicas(js.clone(), storage_key(id), 1);
    worker::append_admitted_fixture_user(&log, "older", "older request").await?;
    log.append_event_async(&SessionLogEntry::Message {
        id: Some("old-assistant".into()),
        role: MessageRole::Assistant,
        content: harnx_core::message::MessageContent::Text("older answer".into()),
        timestamp: None,
        fence_token: None,
    })
    .await?;
    worker::append_admitted_fixture_user(&log, "remove", "remove hex").await?;
    Ok(log)
}

async fn run_compaction_scenario(queue_new_input: bool) -> Result<()> {
    let server = require_nats_server()
        .await?
        .expect("compaction regression requires nats-server");
    let summarizer = Arc::new(GatedSummarizer::new());
    let _state = TestStateGuard::new(Some(summarizer.clone())).await;
    let config = local_nats_runtime_config(server.url());
    config.write().stream = false;
    let requests = Arc::new(Mutex::new(TurnRequests::default()));
    let daemon = spawn_worker_daemon_with_call_fn(
        config,
        "compaction-worker",
        capture_turns(requests.clone()),
    )
    .await?;
    // Abort on assertion/timeout too, so a failed fixture doesn't leak a daemon.
    let _cleanup = scopeguard::guard(daemon.abort_handle(), |handle| handle.abort());
    let js = local_test_nats(server.url()).await?;
    let id = "compaction-continuity";
    let log = seed_history(&js, id).await?;
    let before = harnx_runtime::nats_metrics::snapshot().lease_acquisitions;
    activate_session(&js, id).await?;
    // The first turn's assistant is durable; its automatic summarizer has captured
    // the old prefix and retained completed turn but hasn't re-logged them yet.
    if tokio::time::timeout(CI_SAFE_TIMEOUT, summarizer.entered.notified())
        .await
        .is_err()
    {
        let entries = log.load_events_async().await?;
        anyhow::bail!("automatic summarizer wasn't called: prompts={:?}, daemon_finished={}, entries={entries:?}", requests.lock().unwrap().prompts, daemon.is_finished());
    }
    if queue_new_input {
        worker::append_admitted_fixture_user(&log, "export", "fix export").await?;
    }
    summarizer.release.notify_one();
    wait_for_worker_daemon_idle(&js, id, before).await?;
    let entries = log.load_events_async().await?;
    assert_scenario(&requests.lock().unwrap(), &entries, queue_new_input);
    assert_eq!(
        summarizer.calls.load(Ordering::SeqCst),
        if queue_new_input { 2 } else { 1 }
    );
    daemon.abort();
    let _ = daemon.await;
    Ok(())
}

fn assert_scenario(requests: &TurnRequests, entries: &[(u64, SessionLogEntry)], queued: bool) {
    let expected = if queued {
        vec!["remove hex", "fix export"]
    } else {
        vec!["remove hex"]
    };
    assert_eq!(
        requests.prompts, expected,
        "copied completed prompts must not schedule turns"
    );
    let compact_count = entries
        .iter()
        .filter(|(_, e)| matches!(e, SessionLogEntry::Compress { .. }))
        .count();
    assert_eq!(
        compact_count,
        expected.len(),
        "automatic compaction ran after each real turn"
    );
    let remove_copies = entries
        .iter()
        .filter(|(_, e)| {
            matches!(e,
        SessionLogEntry::Message { id: Some(id), role: MessageRole::User, .. } if id == "remove")
        })
        .count();
    assert_eq!(
        remove_copies, 2,
        "fixture must reproduce the re-logged completed prompt"
    );
    if queued {
        let messages = &requests.messages[1];
        assert_eq!(
            messages.len(),
            4,
            "continuation provider context: {messages:?}"
        );
        assert_eq!(messages[0].0, MessageRole::User);
        assert!(messages[0]
            .1
            .starts_with("[Runtime note] Earlier conversation summary:\n\nolder work completed"));
        assert_eq!(
            &messages[1..],
            &[
                (MessageRole::User, "remove hex".into()),
                (MessageRole::Assistant, "completed: remove hex".into()),
                (MessageRole::User, "fix export".into()),
            ]
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn automatic_compaction_drains_without_repeating_completed_prompt() -> Result<()> {
    run_compaction_scenario(false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn automatic_compaction_runs_concurrent_new_input_once() -> Result<()> {
    run_compaction_scenario(true).await
}
