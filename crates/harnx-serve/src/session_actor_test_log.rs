use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use harnx_core::session::SessionLogEntry;
use harnx_runtime::config::session::SessionAppendSink;

use super::SessionKey;

#[derive(Default)]
pub(super) struct TestSessionLog(Mutex<Vec<SessionLogEntry>>);

impl TestSessionLog {
    pub(super) fn entries(&self) -> Vec<SessionLogEntry> {
        self.0.lock().expect("test session log poisoned").clone()
    }
}

impl SessionAppendSink for TestSessionLog {
    fn append(&self, entry: &SessionLogEntry) -> anyhow::Result<u64> {
        let mut entries = self.0.lock().expect("test session log poisoned");
        entries.push(entry.clone());
        Ok(entries.len() as u64)
    }
}

type TestSessionLogs = HashMap<String, Arc<TestSessionLog>>;
static TEST_SESSION_LOGS: OnceLock<Mutex<TestSessionLogs>> = OnceLock::new();

fn test_session_log_key(key: &SessionKey) -> String {
    let scope = std::env::var("HARNX_CONFIG_DIR").unwrap_or_default();
    format!("{scope}\0{}\0{}", key.agent, key.session)
}

pub(super) fn test_session_log(key: &SessionKey) -> Arc<TestSessionLog> {
    TEST_SESSION_LOGS
        .get_or_init(Default::default)
        .lock()
        .expect("test session log registry poisoned")
        .entry(test_session_log_key(key))
        .or_default()
        .clone()
}

#[doc(hidden)]
pub fn load_test_session_messages(agent: &str, session: &str) -> Vec<harnx_core::message::Message> {
    let key = SessionKey {
        agent: agent.to_string(),
        session: session.to_string(),
    };
    let entries = test_session_log(&key).entries();
    let raw = entries.into_iter().enumerate().collect::<Vec<_>>();
    harnx_runtime::config::session::replay_log_entries_for_external(&raw, session)
        .expect("replay test session log")
        .messages
}
