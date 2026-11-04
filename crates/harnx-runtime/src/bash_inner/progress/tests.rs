use std::{
    sync::{mpsc, Weak},
    time::Duration,
};

use serde_json::json;

use super::*;
use crate::bash_inner::InnerTraceStatus;

const WAIT: Duration = Duration::from_secs(5);

#[derive(Default)]
struct RecordingProgress(Mutex<Vec<ToolUpdatePatch>>);

impl ToolProgress for RecordingProgress {
    fn update(&self, patch: ToolUpdatePatch) {
        self.0.lock().unwrap().push(patch);
    }
}

fn call(name: &str) -> ToolCall {
    ToolCall::new(name.to_owned(), json!({}), Some(name.to_owned()), None)
}

fn entry(name: &str) -> InnerTraceEntry {
    InnerTraceEntry {
        start_seq: 0,
        call_id: name.to_owned(),
        name: name.to_owned(),
        args_summary: "{}".to_owned(),
        status: InnerTraceStatus::Ok,
        bytes: 42,
        duration_ms: 7,
    }
}

struct GatedProgress {
    patches: RecordingProgress,
    entered: mpsc::Sender<()>,
    release: Mutex<mpsc::Receiver<()>>,
}

impl ToolProgress for GatedProgress {
    fn update(&self, patch: ToolUpdatePatch) {
        if patch.title.as_deref() == Some("bash: first (0 done, 1 running)") {
            self.entered.send(()).unwrap();
            self.release.lock().unwrap().recv_timeout(WAIT).unwrap();
        }
        self.patches.update(patch);
    }
}

#[test]
fn concurrent_callbacks_publish_in_order_without_holding_the_state_lock() {
    let (entered_tx, entered) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    let progress = Arc::new(GatedProgress {
        patches: RecordingProgress::default(),
        entered: entered_tx,
        release: Mutex::new(release_rx),
    });
    let aggregator = Arc::new(InnerProgressAggregator::new(progress.clone()));
    let publisher = aggregator.clone();
    let thread = std::thread::spawn(move || publisher.started(&call("first"), &json!({})));
    entered.recv_timeout(WAIT).unwrap();

    // The first sink call is paused. Neither callback may block on that sink,
    // and neither may publish a newer snapshot ahead of the paused one.
    aggregator.started(&call("second"), &json!({}));
    aggregator.finished(&entry("first"));
    release.send(()).unwrap();
    thread.join().unwrap();
    aggregator.finished(&entry("second"));

    let patches = progress.patches.0.lock().unwrap();
    let titles: Vec<_> = patches.iter().map(|patch| patch.title.as_deref()).collect();
    assert_eq!(
        titles,
        [
            Some("bash: first (0 done, 1 running)"),
            Some("bash: second (1 done, 1 running)"),
            Some("bash: (2 done, 0 running)"),
        ]
    );
    assert_eq!(
        patches.last().unwrap().markdown.as_deref(),
        Some("first ok 42B 7ms\nsecond ok 42B 7ms")
    );
}

#[derive(Default)]
struct ReentrantProgress {
    aggregator: Mutex<Weak<InnerProgressAggregator>>,
    patches: RecordingProgress,
}

impl ToolProgress for ReentrantProgress {
    fn update(&self, patch: ToolUpdatePatch) {
        let started = patch.title.as_deref() == Some("bash: first (0 done, 1 running)");
        self.patches.update(patch);
        if started {
            let aggregator = self.aggregator.lock().unwrap().upgrade().unwrap();
            // A synchronous emitter can call back into the observer.
            aggregator.finished(&entry("first"));
        }
    }
}

#[test]
fn synchronous_sink_reentry_publishes_the_latest_state() {
    let progress = Arc::new(ReentrantProgress::default());
    let aggregator = Arc::new(InnerProgressAggregator::new(progress.clone()));
    *progress.aggregator.lock().unwrap() = Arc::downgrade(&aggregator);
    let (done_tx, done) = mpsc::channel();
    std::thread::spawn(move || {
        aggregator.started(&call("first"), &json!({}));
        done_tx.send(()).unwrap();
    });
    done.recv_timeout(WAIT).unwrap();

    let patches = progress.patches.0.lock().unwrap();
    let titles: Vec<_> = patches.iter().map(|patch| patch.title.as_deref()).collect();
    assert_eq!(
        titles,
        [
            Some("bash: first (0 done, 1 running)"),
            Some("bash: (1 done, 0 running)"),
        ]
    );
}

#[test]
fn oversized_trace_lines_cannot_hide_newer_completions() {
    let progress = Arc::new(RecordingProgress::default());
    let aggregator = InnerProgressAggregator::new(progress.clone());
    // Both names and summaries can grow a rendered line. Multi-byte characters
    // exercise truncation at character boundaries as well as the byte budget.
    let oversized = "é".repeat(AGGREGATOR_MAX_MARKDOWN_BYTES);
    let mut oversized_args = entry("oversized_args");
    oversized_args.args_summary = oversized.clone();
    aggregator.finished(&entry(&oversized));
    aggregator.finished(&oversized_args);
    for index in 0..AGGREGATOR_MAX_TRACE_LINES - 2 {
        aggregator.finished(&entry(&format!("newer_{index}")));
    }

    let patches = progress.0.lock().unwrap();
    let markdown = patches.last().unwrap().markdown.as_deref().unwrap();
    let lines: Vec<_> = markdown.split('\n').collect();
    assert_trace_budget(markdown);
    assert_eq!(lines.len(), AGGREGATOR_MAX_TRACE_LINES);
    assert_eq!(lines.last(), Some(&"newer_7 ok 42B 7ms"));
}

#[track_caller]
fn assert_trace_budget(markdown: &str) {
    assert!(markdown.len() <= AGGREGATOR_MAX_MARKDOWN_BYTES);
    assert!(markdown
        .split('\n')
        .all(|line| line.len() <= AGGREGATOR_MAX_TRACE_LINE_BYTES));
}

#[test]
fn truncate_bytes_respects_char_boundaries() {
    let text = "é".repeat(10);
    assert_eq!(text.len(), 20);
    let truncated = truncate_bytes(text.clone(), 5);
    assert_eq!(truncated, "éé".to_string());
    assert!(truncated.len() <= 5);
    assert_eq!(truncate_bytes(text.clone(), 20), text);
}
