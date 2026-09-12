use super::*;
use futures_util::StreamExt;
use harnx_core::{api_types::CompletionTokenUsage, session::ToolOutput};
use serde_json::json;

struct Server {
    child: std::process::Child,
    _directory: tempfile::TempDir,
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn test_server() -> Option<(Server, async_nats::Client)> {
    let (url, child, directory) = crate::nats_worker::tests::spawn_test_nats().await?;
    let server = Server {
        child,
        _directory: directory,
    };
    let client = async_nats::ConnectOptions::new()
        .request_timeout(Some(Duration::from_millis(250)))
        .connect(url)
        .await
        .unwrap();
    Some((server, client))
}

#[tokio::test]
async fn nats_completion_reads_only_entries_after_its_successful_cursor() -> Result<()> {
    let Some((_server, client)) = test_server().await else {
        return Ok(());
    };
    let js = async_nats::jetstream::new(client.clone());
    let log = NatsSessionLog::new(js.clone(), "incremental");
    log.append_event_async(&SessionLogEntry::Cancel { fence_token: 1 })
        .await?;
    let history = log.load_events_async().await?;
    let mut requests = client
        .subscribe("$JS.API.STREAM.MSG.GET.SESSION_INCREMENTAL".to_string())
        .await?;
    client.flush().await?;
    log.append_event_async(&SessionLogEntry::TurnEnd {
        through_seq: 1,
        fence_token: 1,
        timestamp: None,
        usage: None,
    })
    .await?;
    let updates = updates(js, "incremental".into(), history);
    tokio::pin!(updates);
    let update = updates.next().await.unwrap()?;
    assert_eq!(update.entries.len(), 2);
    assert!(matches!(
        update.entries[1].1,
        SessionLogEntry::TurnEnd { .. }
    ));
    let request = tokio::time::timeout(Duration::from_secs(1), requests.next())
        .await?
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&request.payload)?["seq"],
        2
    );
    assert_eq!(updates.next().await.unwrap()?.entries.len(), 2);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), requests.next())
            .await
            .is_err(),
        "unchanged history must not be reread"
    );
    Ok(())
}

#[tokio::test]
async fn nats_completion_reports_persistent_read_failure_without_advisories() -> Result<()> {
    let Some((_server, client)) = test_server().await else {
        return Ok(());
    };
    // The Core NATS connection is healthy but this JetStream API has no
    // responders, as happens when storage is unavailable during an outage.
    let mut js = async_nats::jetstream::with_prefix(client, "UNAVAILABLE");
    js.set_timeout(Duration::from_millis(100));
    let updates = updates(js, "stalled-child".into(), Vec::new());
    tokio::pin!(updates);
    let result = tokio::time::timeout(Duration::from_secs(5), updates.next())
        .await?
        .unwrap();
    let error = match result {
        Ok(_) => panic!("unavailable backend must fail"),
        Err(error) => error,
    };
    let message = format!("{error:#}");
    assert!(message.contains("stalled-child"), "{message}");
    assert!(message.contains("after 3 attempts"), "{message}");
    assert!(message.contains("may still be running"), "{message}");
    Ok(())
}

#[tokio::test]
async fn nats_completion_read_keeps_its_deadline_when_other_select_branches_win() -> Result<()> {
    let Some((_server, client)) = test_server().await else {
        return Ok(());
    };
    // Consume requests without responding to force each attempt to time out.
    let _blackhole = client.subscribe("BLACKHOLE.>".to_string()).await?;
    client.flush().await?;
    let mut js = async_nats::jetstream::with_prefix(client, "BLACKHOLE");
    js.set_timeout(Duration::from_millis(100));
    let updates = updates(js, "busy-advisories".into(), Vec::new());
    tokio::pin!(updates);
    let mut advisories = tokio::time::interval(Duration::from_millis(10));
    let mut received = 0;
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            tokio::select! {
                result = updates.next() => break result.unwrap(),
                _ = advisories.tick() => received += 1,
            }
        }
    })
    .await?;
    assert!(result.is_err());
    assert!(
        received > 10,
        "reads must not block advisory/cancellation polling"
    );
    Ok(())
}

#[derive(Default)]
struct Sink(std::sync::Mutex<Vec<AgentEvent>>);
impl AgentEventSink for Sink {
    fn emit(&self, event: AgentEvent) {
        self.0.lock().unwrap().push(event);
    }
}

#[test]
fn durable_child_completion_repairs_a_lost_event_once_without_ending_parent_turn() {
    let sink = Arc::new(Sink::default());
    let event_sink: Arc<dyn AgentEventSink> = sink.clone();
    let progress = SubAgentProgress {
        invocation_id: "invocation".into(),
        agent: "athena".into(),
        session_id: "child".into(),
        status: SubAgentProgressStatus::Done,
        elapsed_ms: 123,
        usage: CompletionTokenUsage::default(),
        tool_call_count: 3,
    };
    let entries = vec![(
        3,
        SessionLogEntry::ToolResults {
            results: vec![ToolOutput {
                id: Some("call".into()),
                name: "session_prompt".into(),
                output: json!({"response": "done", "sub_agent_progress": progress}),
                markdown: None,
                content: vec![],
                switch_agent: None,
            }],
            timestamp: None,
        },
    )];
    let mut emitted = HashSet::new();
    reconcile_subagent_progress(&entries, 3, &event_sink, &mut emitted);
    assert!(
        sink.0.lock().unwrap().is_empty(),
        "old turns must be excluded"
    );
    for _ in 0..2 {
        reconcile_subagent_progress(&entries, 1, &event_sink, &mut emitted);
    }
    let events = sink.0.lock().unwrap();
    assert!(
        matches!(events.as_slice(), [AgentEvent::Turn(TurnEvent::SubAgentProgress(snapshot))] if snapshot.invocation_id == "invocation" && snapshot.status == SubAgentProgressStatus::Done)
    );
}
