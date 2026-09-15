//! Per-invocation aggregation for NATS-backed sub-agent tools.
//!
//! # Title source for sub-agent progress
//!
//! The `SubAgentProgress.title` field MUST be sourced from session metadata, not from
//! `SessionEvent::TitleUpdated`. Title generation runs in a `tokio::spawn` task that wraps
//! its work with `NullSink` (see `session_ops_title.rs:415-417`). Tokio task-locals are not
//! propagated to spawned tasks, so the child session's scoped event sink is lost and
//! `TitleUpdated` events never reach the parent session's advisory stream.
//!
//! Resumed sub-agent sessions may already have a title from a prior turn with no new event.
//! The metadata store (`SessionMetadataStore::get(session_id).metadata.title.value`) is the
//! canonical source. The reporter reads it at startup, refreshes on the 10s heartbeat, and
//! preserves the last-observed title on read errors.

use crate::nats_event_sink::NatsEventSink;
use crate::nats_session_metadata::SessionMetadataStore;
use async_trait::async_trait;
use harnx_core::api_types::CompletionTokenUsage;
use harnx_core::event::{
    AgentEvent, AgentEventSink, AgentSource, ModelEvent, SubAgentProgress, SubAgentProgressStatus,
    ToolEvent, TurnEvent,
};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

const TITLE_READ_TIMEOUT: Duration = Duration::from_secs(2);

#[async_trait]
trait SessionTitleSource: Send + Sync + 'static {
    async fn get_title(&self, session_id: &str) -> anyhow::Result<Option<String>>;
}

#[async_trait]
impl SessionTitleSource for SessionMetadataStore {
    async fn get_title(&self, session_id: &str) -> anyhow::Result<Option<String>> {
        Ok(self
            .get(session_id)
            .await?
            .and_then(|record| record.metadata.title.value))
    }
}

type TitleRead = Pin<Box<dyn Future<Output = Option<Option<String>>> + Send>>;

struct ReporterTitleConfig {
    source: Option<Arc<dyn SessionTitleSource>>,
    initial: Option<String>,
    read_timeout: Duration,
}

#[derive(Debug)]
enum ProgressMetric {
    Usage(CompletionTokenUsage),
    ToolStarted,
}

impl ProgressMetric {
    /// Nested events belong to their own invocation and are deliberately not
    /// folded into the current child.
    fn from_event(event: AgentEvent) -> Option<Self> {
        match event {
            AgentEvent::Model(ModelEvent::Usage {
                input,
                output,
                cached,
                cache_write,
                ..
            }) => Some(Self::Usage(CompletionTokenUsage {
                input_tokens: input,
                output_tokens: output,
                cached_tokens: cached,
                cache_write_tokens: cache_write,
            })),
            AgentEvent::Tool(ToolEvent::Started { .. }) => Some(Self::ToolStarted),
            AgentEvent::SubAgent { .. } => None,
            _ => None,
        }
    }
}

#[derive(Debug)]
enum ProgressCommand {
    Metric(ProgressMetric),
    Finish {
        status: SubAgentProgressStatus,
        reply: oneshot::Sender<(SubAgentProgress, anyhow::Result<()>)>,
    },
}

struct ProgressEventSink {
    tx: mpsc::UnboundedSender<ProgressCommand>,
}

impl AgentEventSink for ProgressEventSink {
    fn emit(&self, event: AgentEvent) {
        if let Some(metric) = ProgressMetric::from_event(event) {
            let _ = self.tx.send(ProgressCommand::Metric(metric));
        }
    }
}

#[derive(Debug)]
struct ProgressTracker {
    snapshot: SubAgentProgress,
}

impl ProgressTracker {
    fn new(
        agent: String,
        session_id: String,
        invocation_id: String,
        title: Option<String>,
    ) -> Self {
        Self {
            snapshot: SubAgentProgress {
                invocation_id,
                agent,
                session_id,
                status: SubAgentProgressStatus::Running,
                elapsed_ms: 0,
                usage: CompletionTokenUsage::default(),
                tool_call_count: 0,
                title,
            },
        }
    }

    fn apply(&mut self, metric: ProgressMetric, elapsed_ms: u64) -> SubAgentProgress {
        match metric {
            ProgressMetric::Usage(usage) => self.snapshot.usage.accumulate(&usage),
            ProgressMetric::ToolStarted => {
                self.snapshot.tool_call_count = self.snapshot.tool_call_count.saturating_add(1);
            }
        }
        self.snapshot.elapsed_ms = elapsed_ms;
        self.snapshot.clone()
    }

    fn update_title(&mut self, title: Option<String>) {
        self.snapshot.title = title;
    }

    fn heartbeat(&mut self, elapsed_ms: u64) -> SubAgentProgress {
        self.snapshot.elapsed_ms = elapsed_ms;
        self.snapshot.clone()
    }

    fn finish(&mut self, status: SubAgentProgressStatus, elapsed_ms: u64) -> SubAgentProgress {
        self.snapshot.status = status;
        self.snapshot.elapsed_ms = elapsed_ms;
        self.snapshot.clone()
    }
}

struct ReporterTaskConfig {
    agent: String,
    session_id: String,
    invocation_id: String,
    parent_sink: Option<NatsEventSink>,
    title: ReporterTitleConfig,
    heartbeat: Duration,
}

enum ReporterEvent {
    Command(ProgressCommand),
    Heartbeat,
    TitleRefreshed(Option<Option<String>>),
}

struct ReporterTask {
    source: AgentSource,
    session_id: String,
    parent_sink: Option<NatsEventSink>,
    title_source: Option<Arc<dyn SessionTitleSource>>,
    title_read_timeout: Duration,
    tracker: ProgressTracker,
    started: tokio::time::Instant,
    heartbeats: tokio::time::Interval,
    commands: mpsc::UnboundedReceiver<ProgressCommand>,
    title_read: Option<TitleRead>,
}

impl ReporterTask {
    fn new(config: ReporterTaskConfig, commands: mpsc::UnboundedReceiver<ProgressCommand>) -> Self {
        let ReporterTitleConfig {
            source: title_source,
            initial: initial_title,
            read_timeout: title_read_timeout,
        } = config.title;
        let source = AgentSource {
            agent: config.agent.clone(),
            session_id: Some(config.session_id.clone()),
            model: None,
        };
        let tracker = ProgressTracker::new(
            config.agent,
            config.session_id.clone(),
            config.invocation_id,
            initial_title,
        );
        let started = tokio::time::Instant::now();
        let mut heartbeats = tokio::time::interval_at(started + config.heartbeat, config.heartbeat);
        heartbeats.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        Self {
            source,
            session_id: config.session_id,
            parent_sink: config.parent_sink,
            title_source,
            title_read_timeout,
            tracker,
            started,
            heartbeats,
            commands,
            title_read: None,
        }
    }

    async fn run(mut self) {
        while self.process_next_event().await {}
    }

    async fn process_next_event(&mut self) -> bool {
        let Some(event) = self.next_event().await else {
            return false;
        };
        self.handle_event(event).await
    }

    async fn next_event(&mut self) -> Option<ReporterEvent> {
        tokio::select! {
            Some(command) = self.commands.recv() => Some(ReporterEvent::Command(command)),
            _ = self.heartbeats.tick(), if self.title_read.is_none() => {
                Some(ReporterEvent::Heartbeat)
            }
            title = await_title_read(&mut self.title_read), if self.title_read.is_some() => {
                Some(ReporterEvent::TitleRefreshed(title))
            }
            else => None,
        }
    }

    async fn handle_event(&mut self, event: ReporterEvent) -> bool {
        match event {
            ReporterEvent::Command(command) => self.handle_command(command).await,
            ReporterEvent::Heartbeat => {
                self.start_title_refresh();
                true
            }
            ReporterEvent::TitleRefreshed(title) => {
                self.handle_title_refresh(title);
                true
            }
        }
    }

    async fn handle_command(&mut self, command: ProgressCommand) -> bool {
        match command {
            ProgressCommand::Metric(metric) => {
                self.handle_metric(metric);
                true
            }
            ProgressCommand::Finish { status, reply } => {
                self.handle_finish(status, reply).await;
                false
            }
        }
    }

    fn handle_metric(&mut self, metric: ProgressMetric) {
        let snapshot = self.tracker.apply(metric, elapsed_ms(self.started));
        publish_progress(self.parent_sink.as_ref(), &self.source, snapshot);
    }

    async fn handle_finish(
        &mut self,
        status: SubAgentProgressStatus,
        reply: oneshot::Sender<(SubAgentProgress, anyhow::Result<()>)>,
    ) {
        let snapshot = self.tracker.finish(status, elapsed_ms(self.started));
        let delivery =
            publish_terminal_progress(self.parent_sink.as_ref(), &self.source, snapshot.clone())
                .await;
        let _ = reply.send((snapshot, delivery));
    }

    fn start_title_refresh(&mut self) {
        self.title_read = Some(start_title_read(
            self.title_source.clone(),
            self.session_id.clone(),
            self.title_read_timeout,
        ));
    }

    fn handle_title_refresh(&mut self, title: Option<Option<String>>) {
        self.title_read = None;
        if let Some(title) = title {
            self.tracker.update_title(title);
        }
        let snapshot = self.tracker.heartbeat(elapsed_ms(self.started));
        publish_progress(self.parent_sink.as_ref(), &self.source, snapshot);
    }
}

pub(super) struct SubagentProgressReporter {
    sink: Arc<dyn AgentEventSink>,
    tx: mpsc::UnboundedSender<ProgressCommand>,
}

impl SubagentProgressReporter {
    pub(super) async fn start(
        agent: String,
        session_id: String,
        invocation_id: String,
        parent_sink: Option<NatsEventSink>,
        session_metadata: SessionMetadataStore,
        heartbeat: Duration,
    ) -> Self {
        let title_source: Arc<dyn SessionTitleSource> = Arc::new(session_metadata);
        Self::start_with_title_source(
            agent,
            session_id,
            invocation_id,
            parent_sink,
            title_source,
            heartbeat,
            TITLE_READ_TIMEOUT,
        )
        .await
    }

    async fn start_with_title_source(
        agent: String,
        session_id: String,
        invocation_id: String,
        parent_sink: Option<NatsEventSink>,
        title_source: Arc<dyn SessionTitleSource>,
        heartbeat: Duration,
        title_read_timeout: Duration,
    ) -> Self {
        let initial_title = read_title(title_source.as_ref(), &session_id, title_read_timeout)
            .await
            .flatten();
        Self::spawn_reporter(
            agent,
            session_id,
            invocation_id,
            parent_sink,
            ReporterTitleConfig {
                source: Some(title_source),
                initial: initial_title,
                read_timeout: title_read_timeout,
            },
            heartbeat,
        )
    }

    fn spawn_reporter(
        agent: String,
        session_id: String,
        invocation_id: String,
        parent_sink: Option<NatsEventSink>,
        title: ReporterTitleConfig,
        heartbeat: Duration,
    ) -> Self {
        let (tx, commands) = mpsc::unbounded_channel();
        let sink = Arc::new(ProgressEventSink { tx: tx.clone() });
        let config = ReporterTaskConfig {
            agent,
            session_id,
            invocation_id,
            parent_sink,
            title,
            heartbeat,
        };
        tokio::spawn(async move { ReporterTask::new(config, commands).run().await });
        Self { sink, tx }
    }

    #[cfg(test)]
    pub(super) fn spawn(
        agent: String,
        session_id: String,
        invocation_id: String,
        parent_sink: Option<NatsEventSink>,
        heartbeat: Duration,
    ) -> Self {
        Self::spawn_reporter(
            agent,
            session_id,
            invocation_id,
            parent_sink,
            ReporterTitleConfig {
                source: None,
                initial: None,
                read_timeout: TITLE_READ_TIMEOUT,
            },
            heartbeat,
        )
    }

    pub(super) fn sink(&self) -> Arc<dyn AgentEventSink> {
        Arc::clone(&self.sink)
    }

    pub(super) async fn finish(
        &self,
        status: SubAgentProgressStatus,
    ) -> anyhow::Result<SubAgentProgress> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(ProgressCommand::Finish {
                status,
                reply: reply_tx,
            })
            .map_err(|_| anyhow::anyhow!("sub-agent progress reporter stopped early"))?;
        let (snapshot, delivery) = reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("sub-agent progress reporter dropped completion"))?;
        delivery?;
        Ok(snapshot)
    }
}

async fn await_title_read(title_read: &mut Option<TitleRead>) -> Option<Option<String>> {
    title_read
        .as_mut()
        .expect("title read branch requires a pending read")
        .await
}

fn start_title_read(
    source: Option<Arc<dyn SessionTitleSource>>,
    session_id: String,
    timeout: Duration,
) -> TitleRead {
    Box::pin(async move {
        match source {
            Some(source) => read_title(source.as_ref(), &session_id, timeout).await,
            None => Some(None),
        }
    })
}

async fn read_title(
    source: &(impl SessionTitleSource + ?Sized),
    session_id: &str,
    timeout: Duration,
) -> Option<Option<String>> {
    match tokio::time::timeout(timeout, source.get_title(session_id)).await {
        Ok(Ok(title)) => Some(title),
        Ok(Err(error)) => {
            log::debug!("failed to read sub-agent session title for '{session_id}': {error:#}");
            None
        }
        Err(_) => {
            log::debug!("timed out reading sub-agent session title for '{session_id}'");
            None
        }
    }
}

fn elapsed_ms(started: tokio::time::Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn progress_event(source: &AgentSource, snapshot: SubAgentProgress) -> AgentEvent {
    AgentEvent::sub_agent(
        source.clone(),
        AgentEvent::Turn(TurnEvent::SubAgentProgress(snapshot)),
    )
}

fn publish_progress(
    parent_sink: Option<&NatsEventSink>,
    source: &AgentSource,
    snapshot: SubAgentProgress,
) {
    if let Some(parent_sink) = parent_sink {
        parent_sink.emit(progress_event(source, snapshot));
    }
}

async fn publish_terminal_progress(
    parent_sink: Option<&NatsEventSink>,
    source: &AgentSource,
    snapshot: SubAgentProgress,
) -> anyhow::Result<()> {
    let Some(parent_sink) = parent_sink else {
        return Ok(());
    };
    parent_sink.emit_required(progress_event(source, snapshot));
    parent_sink.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::event::{ToolKind, ToolLocation};
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    enum StubTitleResponse {
        Value(Option<String>),
        Error,
        Pending,
    }

    #[derive(Clone)]
    struct StubTitleSource {
        responses: Arc<Mutex<VecDeque<StubTitleResponse>>>,
        reads: Arc<AtomicUsize>,
    }

    impl StubTitleSource {
        fn new(responses: impl IntoIterator<Item = StubTitleResponse>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(responses.into_iter().collect())),
                reads: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn read_count(&self) -> usize {
            self.reads.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl SessionTitleSource for StubTitleSource {
        async fn get_title(&self, _session_id: &str) -> anyhow::Result<Option<String>> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            let response = self
                .responses
                .lock()
                .expect("stub title responses lock")
                .pop_front()
                .unwrap_or(StubTitleResponse::Error);
            match response {
                StubTitleResponse::Value(title) => Ok(title),
                StubTitleResponse::Error => anyhow::bail!("stub metadata read failed"),
                StubTitleResponse::Pending => std::future::pending().await,
            }
        }
    }

    async fn reporter(source: StubTitleSource, heartbeat: Duration) -> SubagentProgressReporter {
        SubagentProgressReporter::start_with_title_source(
            "researcher".into(),
            "session-1".into(),
            "inv-1".into(),
            None,
            Arc::new(source),
            heartbeat,
            Duration::from_millis(100),
        )
        .await
    }

    async fn wait_for_reads(source: &StubTitleSource, expected: usize) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while source.read_count() < expected {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("title source was not read in time");
    }

    fn tracker() -> ProgressTracker {
        ProgressTracker::new(
            "researcher".into(),
            "session-1".into(),
            "inv-1".into(),
            None,
        )
    }

    #[tokio::test]
    async fn seeds_title_from_metadata() {
        let source = StubTitleSource::new([StubTitleResponse::Value(Some("Seeded title".into()))]);
        let reporter = reporter(source.clone(), Duration::from_secs(60)).await;

        let terminal = reporter
            .finish(SubAgentProgressStatus::Done)
            .await
            .expect("finish reporter");

        assert_eq!(terminal.title.as_deref(), Some("Seeded title"));
        assert_eq!(source.read_count(), 1);
    }

    #[tokio::test]
    async fn refreshes_title_on_heartbeat() {
        let source = StubTitleSource::new([
            StubTitleResponse::Value(Some("Initial title".into())),
            StubTitleResponse::Value(Some("Refreshed title".into())),
            StubTitleResponse::Value(Some("Refreshed title".into())),
        ]);
        let reporter = reporter(source.clone(), Duration::from_millis(10)).await;
        wait_for_reads(&source, 3).await;

        let terminal = reporter
            .finish(SubAgentProgressStatus::Done)
            .await
            .expect("finish reporter");

        assert_eq!(terminal.title.as_deref(), Some("Refreshed title"));
    }

    #[tokio::test]
    async fn usage_update_preserves_cached_title_without_reading_metadata() {
        let source = StubTitleSource::new([StubTitleResponse::Value(Some("Cached title".into()))]);
        let reporter = reporter(source.clone(), Duration::from_secs(60)).await;
        reporter.sink().emit(AgentEvent::Model(ModelEvent::Usage {
            input: 8,
            output: 3,
            cached: 2,
            cache_write: 1,
            session_label: None,
        }));

        let terminal = reporter
            .finish(SubAgentProgressStatus::Done)
            .await
            .expect("finish reporter");

        assert_eq!(terminal.title.as_deref(), Some("Cached title"));
        assert_eq!(terminal.usage.input_tokens, 8);
        assert_eq!(source.read_count(), 1);
    }

    #[tokio::test]
    async fn metadata_read_error_preserves_prior_title() {
        let source = StubTitleSource::new([
            StubTitleResponse::Value(Some("Last observed title".into())),
            StubTitleResponse::Error,
            StubTitleResponse::Error,
        ]);
        let reporter = reporter(source.clone(), Duration::from_millis(10)).await;
        wait_for_reads(&source, 3).await;

        let terminal = reporter
            .finish(SubAgentProgressStatus::Done)
            .await
            .expect("finish reporter");

        assert_eq!(terminal.title.as_deref(), Some("Last observed title"));
    }

    #[tokio::test]
    async fn metadata_read_timeout_preserves_prior_title() {
        let source = StubTitleSource::new([
            StubTitleResponse::Value(Some("Last observed title".into())),
            StubTitleResponse::Pending,
            StubTitleResponse::Error,
        ]);
        let reporter = SubagentProgressReporter::start_with_title_source(
            "researcher".into(),
            "session-1".into(),
            "inv-1".into(),
            None,
            Arc::new(source.clone()),
            Duration::from_millis(5),
            Duration::from_millis(5),
        )
        .await;
        wait_for_reads(&source, 3).await;

        let terminal = reporter
            .finish(SubAgentProgressStatus::Done)
            .await
            .expect("finish reporter");

        assert_eq!(terminal.title.as_deref(), Some("Last observed title"));
    }

    #[tokio::test]
    async fn pending_metadata_read_does_not_block_reporter_shutdown() {
        let source = StubTitleSource::new([
            StubTitleResponse::Value(Some("Last observed title".into())),
            StubTitleResponse::Pending,
        ]);
        let reporter = SubagentProgressReporter::start_with_title_source(
            "researcher".into(),
            "session-1".into(),
            "inv-1".into(),
            None,
            Arc::new(source.clone()),
            Duration::from_millis(10),
            Duration::from_secs(60),
        )
        .await;
        wait_for_reads(&source, 2).await;

        let terminal = tokio::time::timeout(
            Duration::from_millis(100),
            reporter.finish(SubAgentProgressStatus::Done),
        )
        .await
        .expect("pending title read blocked reporter shutdown")
        .expect("finish reporter");

        assert_eq!(terminal.title.as_deref(), Some("Last observed title"));
    }

    #[test]
    fn aggregates_usage_and_direct_tool_starts() {
        let mut tracker = tracker();
        tracker.apply(
            ProgressMetric::Usage(CompletionTokenUsage::new(Some(10), Some(4), Some(3))),
            20,
        );
        tracker.apply(
            ProgressMetric::Usage(CompletionTokenUsage::new(Some(6), Some(2), Some(1))),
            40,
        );
        let snapshot = tracker.apply(ProgressMetric::ToolStarted, 50);

        assert_eq!(snapshot.usage.input_tokens, 16);
        assert_eq!(snapshot.usage.output_tokens, 6);
        assert_eq!(snapshot.usage.cached_tokens, 4);
        assert_eq!(snapshot.tool_call_count, 1);
        assert_eq!(snapshot.elapsed_ms, 50);
    }

    #[test]
    fn usage_event_preserves_cache_write_tokens() {
        let event = AgentEvent::Model(ModelEvent::Usage {
            input: 12,
            output: 4,
            cached: 5,
            cache_write: 3,
            session_label: None,
        });

        let Some(ProgressMetric::Usage(usage)) = ProgressMetric::from_event(event) else {
            panic!("expected usage metric");
        };
        assert_eq!(
            usage,
            CompletionTokenUsage {
                input_tokens: 12,
                output_tokens: 4,
                cached_tokens: 5,
                cache_write_tokens: 3,
            }
        );
    }

    #[test]
    fn ignores_nested_agent_metrics() {
        let nested = AgentEvent::sub_agent(
            AgentSource {
                agent: "nested".into(),
                session_id: Some("nested-session".into()),
                model: None,
            },
            AgentEvent::Model(ModelEvent::Usage {
                input: 99,
                output: 88,
                cached: 77,
                cache_write: 66,
                session_label: None,
            }),
        );
        assert!(ProgressMetric::from_event(nested).is_none());
    }

    #[test]
    fn counts_delegation_tools_started_by_the_direct_session() {
        let event = AgentEvent::Tool(ToolEvent::Started {
            id: "call-1".into(),
            name: "reviewer_session_prompt".into(),
            kind: ToolKind::Other,
            markdown: None,
            input: serde_json::json!({}),
            locations: Vec::<ToolLocation>::new(),
        });
        assert!(matches!(
            ProgressMetric::from_event(event),
            Some(ProgressMetric::ToolStarted)
        ));
    }

    #[test]
    fn heartbeat_updates_elapsed_without_changing_metrics() {
        let mut tracker = tracker();
        tracker.apply(ProgressMetric::ToolStarted, 5);
        let heartbeat = tracker.heartbeat(10_000);

        assert_eq!(heartbeat.status, SubAgentProgressStatus::Running);
        assert_eq!(heartbeat.elapsed_ms, 10_000);
        assert_eq!(heartbeat.tool_call_count, 1);
    }

    #[test]
    fn terminal_snapshot_freezes_done_or_failed_state() {
        let mut done = tracker();
        let done = done.finish(SubAgentProgressStatus::Done, 12_345);
        assert_eq!(done.status, SubAgentProgressStatus::Done);
        assert_eq!(done.elapsed_ms, 12_345);

        let mut failed = tracker();
        let failed = failed.finish(SubAgentProgressStatus::Failed, 98);
        assert_eq!(failed.status, SubAgentProgressStatus::Failed);
        assert_eq!(failed.elapsed_ms, 98);
    }
}
