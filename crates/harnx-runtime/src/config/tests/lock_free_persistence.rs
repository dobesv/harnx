//! Session persistence must not hold the config lock across a NATS round
//! trip. A Tokio worker blocked on that lock can strand the runtime's I/O
//! driver, and then the reply the holder waits for is never read.

use crate::config::session::SessionAppendSink;
use crate::config::*;
use crate::nats_session_metadata::SessionOverrideUpdate;
use harnx_core::message::{Message, MessageContent, MessageRole};
use harnx_core::session::SessionLogEntry;
use std::sync::{Mutex, OnceLock, Weak};

/// A sink that notes, at every persistence call, whether the config lock
/// was free: a caller still holding a guard makes `try_write` fail.
#[derive(Default)]
struct LockProbeSink {
    config: OnceLock<Weak<ConfigLock>>,
    calls: Mutex<Vec<(&'static str, bool)>>,
    entries: Mutex<Vec<SessionLogEntry>>,
}

impl LockProbeSink {
    fn probe(&self, call: &'static str) {
        let lock_free = self
            .config
            .get()
            .and_then(Weak::upgrade)
            .is_some_and(|config| config.try_write().is_some());
        self.calls.lock().unwrap().push((call, lock_free));
    }

    fn calls(&self) -> Vec<(&'static str, bool)> {
        self.calls.lock().unwrap().clone()
    }

    fn assert_called_without_lock(&self, call: &'static str) {
        let calls = self.calls();
        assert!(
            calls.iter().any(|(name, _)| *name == call),
            "{call} was never persisted: {calls:?}"
        );
        assert!(
            calls.iter().all(|(_, lock_free)| *lock_free),
            "persisted while holding the config lock: {calls:?}"
        );
    }
}

impl SessionAppendSink for LockProbeSink {
    fn append(&self, entry: &SessionLogEntry) -> anyhow::Result<u64> {
        self.probe("append");
        let mut entries = self.entries.lock().unwrap();
        entries.push(entry.clone());
        Ok(entries.len() as u64)
    }

    fn persist_title(&self, _title: &str, _manual: bool, _tokens: usize) -> anyhow::Result<()> {
        self.probe("persist_title");
        Ok(())
    }

    fn persist_override(&self, _update: &SessionOverrideUpdate) -> anyhow::Result<()> {
        self.probe("persist_override");
        Ok(())
    }
}

fn probed_config(
    build: impl FnOnce(&mut harnx_core::session::Session),
) -> (GlobalConfig, Arc<LockProbeSink>) {
    let mut config = Config::default();
    let mut session = session::new(&config, "lock-probe", None).unwrap();
    build(&mut session);
    let sink = Arc::new(LockProbeSink::default());
    session.runtime = Some(Arc::new(sink.clone() as Arc<dyn SessionAppendSink>));
    config.session = Some(session);
    let config = Arc::new(ConfigLock::new(config));
    sink.config.set(Arc::downgrade(&config)).unwrap();
    (config, sink)
}

/// A finished tool round, which the log records as `ToolCalls` followed by
/// `ToolResults`.
fn tool_round(tool: &str, output: &str) -> Message {
    let call = crate::tool::ToolCall::new(
        tool.to_string(),
        serde_json::json!({}),
        Some(format!("{tool}-call")),
        None,
    );
    let results = vec![crate::tool::ToolResult::new(
        call,
        serde_json::json!(output),
    )];
    Message::new(
        MessageRole::Tool,
        MessageContent::ToolCalls(crate::client::MessageContentToolCalls::new(
            results,
            String::new(),
            None,
        )),
    )
}

fn generated(title: &str, tokens: usize) -> session::TitleRecord {
    session::TitleRecord {
        title: title.to_string(),
        manual: false,
        tokens,
    }
}

fn active_session_id(config: &GlobalConfig) -> String {
    config.read().session.as_ref().unwrap().id.clone()
}

#[test]
fn generated_title_is_persisted_without_the_config_lock() {
    let (config, sink) = probed_config(|_| {});
    let session_id = active_session_id(&config);

    let recorded = session::record_title(&config, &session_id, generated("Generated", 42)).unwrap();

    assert!(recorded);
    sink.assert_called_without_lock("persist_title");
    let guard = config.read();
    let session = guard.session.as_ref().unwrap();
    assert_eq!(session.title(), Some("Generated"));
    assert_eq!(session.title_last_updated_tokens(), 42);
}

#[test]
fn title_for_an_inactive_session_is_neither_persisted_nor_applied() {
    let (config, sink) = probed_config(|_| {});

    let recorded =
        session::record_title(&config, "some-other-session", generated("Stale", 1)).unwrap();

    assert!(!recorded);
    assert!(sink.calls().is_empty());
    assert_eq!(config.read().session.as_ref().unwrap().title(), None);
}

#[test]
fn manual_title_is_persisted_without_the_config_lock() {
    let (config, sink) = probed_config(|_| {});

    Config::update(&config, "title Chosen by hand").unwrap();

    sink.assert_called_without_lock("persist_title");
    let guard = config.read();
    let session = guard.session.as_ref().unwrap();
    assert_eq!(session.title(), Some("Chosen by hand"));
    assert_eq!(session.title_last_updated_tokens(), usize::MAX);
}

#[test]
fn setting_override_is_persisted_without_the_config_lock() {
    let (config, sink) = probed_config(|_| {});

    Config::update(&config, "temperature 0.5").unwrap();

    sink.assert_called_without_lock("persist_override");
    assert_eq!(
        config.read().session.as_ref().unwrap().temperature(),
        Some(0.5)
    );
}

#[test]
fn compaction_appends_without_the_config_lock() {
    let (config, sink) = probed_config(|session| {
        session.push_message_for_test(MessageRole::User, "old question".to_string());
        session.push_message_for_test(MessageRole::Assistant, "old answer".to_string());
        session.push_message_for_test(MessageRole::User, "recent question".to_string());
        session.messages.push(tool_round("lookup", "found it"));
        session.push_message_for_test(MessageRole::Assistant, "recent answer".to_string());
    });
    let session_id = active_session_id(&config);

    assert!(Config::apply_compaction_summary(
        &config,
        &session_id,
        "summary".to_string(),
        2
    ));

    sink.assert_called_without_lock("append");
    let entries = sink.entries.lock().unwrap().clone();
    assert!(
        matches!(entries.as_slice(), [
            SessionLogEntry::Compress { prompt },
            SessionLogEntry::Message { role: MessageRole::User, .. },
            SessionLogEntry::ToolCalls { .. },
            SessionLogEntry::ToolResults { .. },
            SessionLogEntry::Message { role: MessageRole::Assistant, .. },
        ] if prompt == "summary"),
        "unexpected compaction log: {entries:?}"
    );
    let guard = config.read();
    let session = guard.session.as_ref().unwrap();
    assert_eq!(session.compaction_summary.as_deref(), Some("summary"));
    assert_eq!(session.compressed_messages.len(), 2);
    let kept_seqs: Vec<_> = session.messages.iter().map(|m| m.log_seq).collect();
    // A message's log seq is the log length before its first entry. The
    // kept messages follow the `Compress` marker at seq 1, and the tool round
    // takes two entries.
    assert_eq!(kept_seqs, vec![Some(1), Some(2), Some(4)]);
    assert_eq!(session.log_entry_count, 5);
    assert!(!session.dirty);
}

#[test]
fn maintenance_poll_reads_a_held_lock_as_pending() {
    let (config, _sink) = probed_config(|_| {});
    assert!(!Config::session_maintenance_pending(&config, |_| false));

    let _guard = config.write();
    assert!(Config::session_maintenance_pending(&config, |_| false));
}
