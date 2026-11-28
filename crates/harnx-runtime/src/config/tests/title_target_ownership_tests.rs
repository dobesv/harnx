//! Title-write ownership (cycle 2 finding C03a): a `.set title` in flight
//! while an already-running `.session` replaces the shared config must
//! persist through the captured destination exactly once, and must not
//! update the replacement session's memory, report success for it, or emit
//! `TitleUpdated` for it — including a replacement that reuses the short
//! session id with a different agent, and so a different storage key.

use crate::config::session::SessionAppendSink;
use crate::config::*;
use harnx_core::event::{AgentEvent, AgentEventSink, SessionEvent};
use harnx_core::session::SessionLogEntry;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A sink whose `persist_title` blocks until the test releases it, so a
/// session replacement can be interleaved exactly between the destination
/// capture and the completion. Every call is recorded.
struct GatedSink {
    started: std::sync::mpsc::Sender<()>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
    titles: Mutex<Vec<(String, bool, usize)>>,
}

impl GatedSink {
    fn new(started: std::sync::mpsc::Sender<()>, release: std::sync::mpsc::Receiver<()>) -> Self {
        Self {
            started,
            release: Mutex::new(release),
            titles: Mutex::new(Vec::new()),
        }
    }

    fn titles(&self) -> Vec<(String, bool, usize)> {
        self.titles.lock().unwrap().clone()
    }
}

impl SessionAppendSink for GatedSink {
    fn append(&self, _entry: &SessionLogEntry) -> anyhow::Result<u64> {
        Ok(1)
    }

    // Blocks the worker thread the write future runs on; the tests below use
    // a multi-thread runtime so the test body keeps running.
    fn persist_title(&self, title: &str, manual: bool, tokens: usize) -> anyhow::Result<()> {
        self.titles
            .lock()
            .unwrap()
            .push((title.to_string(), manual, tokens));
        let _ = self.started.send(());
        let _ = self.release.lock().unwrap().recv();
        Ok(())
    }
}

/// A plain recording sink: proves the replacement session's own sink was
/// never used for the captured write.
#[derive(Default)]
struct RecordingSink {
    titles: Mutex<Vec<String>>,
}

impl SessionAppendSink for RecordingSink {
    fn append(&self, _entry: &SessionLogEntry) -> anyhow::Result<u64> {
        Ok(1)
    }

    fn persist_title(&self, title: &str, _manual: bool, _tokens: usize) -> anyhow::Result<()> {
        self.titles.lock().unwrap().push(title.to_string());
        Ok(())
    }
}

/// Captures `TitleUpdated` emissions; the write path may only emit after the
/// full-target fence passes.
#[derive(Default)]
struct EventRecorder {
    titles: Mutex<Vec<String>>,
}

impl AgentEventSink for EventRecorder {
    fn emit(&self, event: AgentEvent) {
        if let AgentEvent::Session(SessionEvent::TitleUpdated(title)) = event {
            self.titles.lock().unwrap().push(title);
        }
    }
}

fn install_title_event_recorder() -> Arc<EventRecorder> {
    let recorder = Arc::new(EventRecorder::default());
    harnx_core::sink::install_agent_event_sink(recorder.clone());
    recorder
}

/// A config whose active session has short id `shared`, belonging to
/// `remote_agent`, optionally with a persistence sink installed.
fn config_with_session(
    remote_agent: Option<(&str, &str)>,
    sink: Option<Arc<dyn SessionAppendSink>>,
) -> GlobalConfig {
    let config = Config {
        remote_agent: remote_agent.map(|(agent, cluster)| (agent.to_string(), cluster.to_string())),
        ..Config::default()
    };
    let mut session = session::new(&config, "shared", None).unwrap();
    if let Some(sink) = sink {
        // `session::sink()` downcasts runtime to `Arc<dyn SessionAppendSink>`,
        // so the sink Arc goes inside a second Arc, like every real install.
        session.runtime = Some(Arc::new(sink));
    }
    Arc::new(ConfigLock::new(Config {
        session: Some(session),
        ..config
    }))
}

/// The `.session`-style in-place replacement: install a new session object
/// with the same short id, built for `remote_agent`, under `sink`.
fn replace_active_session(
    global: &GlobalConfig,
    remote_agent: (&str, &str),
    sink: Option<Arc<dyn SessionAppendSink>>,
) {
    let replacement = config_with_session(Some(remote_agent), sink);
    let mut guard = global.write();
    guard.remote_agent = replacement.read().remote_agent.clone();
    guard.session = replacement.read().session.clone();
}

fn wait_for_persist_start(rx: &std::sync::mpsc::Receiver<()>) {
    rx.recv_timeout(Duration::from_secs(10))
        .expect("persist_title call started");
}

/// A durable `.set title` write parked inside its captured sink's
/// `persist_title`, ready for the test to replace the active session
/// mid-write.
struct InFlightTitleWrite {
    global: GlobalConfig,
    sink: Arc<GatedSink>,
    release: std::sync::mpsc::Sender<()>,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

/// Spawn `Config::update_setting_durably` for `title` on a config whose
/// gated sink parks the write, and wait until it is provably in flight.
async fn start_gated_title_write(remote_agent: (&str, &str), title: &str) -> InFlightTitleWrite {
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let sink = Arc::new(GatedSink::new(started_tx, release_rx));
    let global = config_with_session(
        Some(remote_agent),
        Some(sink.clone() as Arc<dyn SessionAppendSink>),
    );
    let task = tokio::spawn({
        let global = global.clone();
        let command = format!("title {title}");
        async move { Config::update_setting_durably(&global, &command, None).await }
    });
    wait_for_persist_start(&started_rx);
    InFlightTitleWrite {
        global,
        sink,
        release: release_tx,
        task,
    }
}

impl InFlightTitleWrite {
    /// Release the sink and require the completion to be rejected as a
    /// stale write: no success report for a replaced target. Returns the
    /// config and captured sink for the caller's post-write assertions.
    async fn finish_expecting_stale(self) -> (GlobalConfig, Arc<GatedSink>) {
        self.release.send(()).expect("release persist_title");
        let error = self
            .task
            .await
            .unwrap()
            .expect_err("stale completion must not report success");
        assert!(
            error.to_string().contains("active session changed"),
            "unexpected error: {error:#}"
        );
        (self.global, self.sink)
    }

    /// Release the sink and require the same-target completion to succeed.
    async fn finish_expecting_success(self) -> (GlobalConfig, Arc<GatedSink>) {
        self.release.send(()).expect("release persist_title");
        self.task
            .await
            .unwrap()
            .expect("same-target completion must succeed");
        (self.global, self.sink)
    }
}

/// The active session's in-memory title state: untouched by the fenced write.
fn assert_memory_untitled(global: &GlobalConfig) {
    let guard = global.read();
    let session = guard.session.as_ref().unwrap();
    assert_eq!(session.title(), None);
    assert_eq!(session.title_last_updated_tokens(), 0);
}

/// Same short session id, different agent (so a different storage key) and a
/// different sink: the captured write persists once through the captured
/// sink, and neither the replacement's sink, memory, success report, nor
/// `TitleUpdated` sees it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_id_cross_target_switch_fences_memory_and_event() {
    let recorder = install_title_event_recorder();
    let write = start_gated_title_write(("agent-a", "local"), "A Captured Title").await;

    // The already-running `.session` replacement: same short id, different
    // agent with its own sink — a different storage key.
    let sink_b = Arc::new(RecordingSink::default());
    replace_active_session(
        &write.global,
        ("agent-b", "local"),
        Some(sink_b.clone() as Arc<dyn SessionAppendSink>),
    );
    let (global, sink_a) = write.finish_expecting_stale().await;

    // The captured destination was used exactly once; the replacement's
    // sink was never consulted.
    assert_eq!(
        sink_a.titles(),
        vec![("A Captured Title".to_string(), true, 0)]
    );
    assert!(sink_b.titles.lock().unwrap().is_empty());
    // B's memory never received A's title.
    {
        let guard = global.read();
        assert_eq!(
            guard.session.as_ref().unwrap().agent_name(),
            Some("agent-b")
        );
    }
    assert_memory_untitled(&global);
    // No TitleUpdated was accepted for the wrong target.
    assert!(recorder.titles.lock().unwrap().is_empty());
    harnx_core::sink::clear_agent_event_sink();
}

/// Same id and agent (so the same storage key) but no installed sink after a
/// read-only re-select: the captured sink still persists once — the write
/// must not silently skip persistence and claim success — and the
/// same-target fence lets memory and the event through.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sink_detached_after_capture_still_persists_captured_destination_once() {
    let recorder = install_title_event_recorder();
    let write = start_gated_title_write(("agent-a", "local"), "Detached Target").await;

    // Same target re-selected with no sink (a read-only history load
    // deliberately installs none).
    replace_active_session(&write.global, ("agent-a", "local"), None);
    let (global, sink_a) = write.finish_expecting_success().await;

    assert_eq!(
        sink_a.titles(),
        vec![("Detached Target".to_string(), true, 0)]
    );
    {
        let guard = global.read();
        let session = guard.session.as_ref().unwrap();
        assert_eq!(session.title(), Some("Detached Target"));
        // Manual title: regeneration is frozen.
        assert_eq!(session.title_last_updated_tokens(), usize::MAX);
    }
    assert_eq!(
        recorder.titles.lock().unwrap().clone(),
        vec!["Detached Target".to_string()]
    );
    harnx_core::sink::clear_agent_event_sink();
}

/// Same id and agent but the agent was re-routed to another cluster between
/// capture and completion: the full-target fence (id, storage key,
/// cluster) rejects the memory update and the event, and the write is
/// reported as a stale completion instead of success.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cluster_reroute_after_capture_fences_memory_and_event() {
    let recorder = install_title_event_recorder();
    let write = start_gated_title_write(("agent-a", "cluster-a"), "Rerouted").await;

    // The `.session` switch re-routed the agent to another cluster while
    // leaving the same session installed.
    write.global.write().remote_agent = Some(("agent-a".to_string(), "cluster-b".to_string()));

    let (global, sink_a) = write.finish_expecting_stale().await;

    assert_eq!(sink_a.titles(), vec![("Rerouted".to_string(), true, 0)]);
    assert_memory_untitled(&global);
    assert!(recorder.titles.lock().unwrap().is_empty());
    harnx_core::sink::clear_agent_event_sink();
}
