//! Integration coverage for frontend-targeted local NATS workers.

mod common;

use anyhow::{Context, Result};
use common::spawn_nats_server;
use futures_util::StreamExt;
use harnx_core::{
    event::NullSink,
    message::{MessageContent, MessageRole},
    require_nextest,
    session::SessionLogEntry,
};
use harnx_runtime::{
    client::CompletionTokenUsage,
    config::Config,
    nats_lease::NatsLeaseConfig,
    nats_session_log::NatsSessionLog,
    nats_worker::{
        publish_targeted_session_activate, run_worker_daemon, targeted_consumer_name,
        targeted_notify_subject, targeted_worker_ready_subject, LocalWorkerTarget, SessionActivate,
        WorkerDaemonConfig,
    },
    utils::create_abort_signal,
    NatsSession, NatsSessionConfig,
};
use parking_lot::RwLock;
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::Notify;

const CI_SAFE_TIMEOUT: Duration = Duration::from_secs(60);

struct EnvVarGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn set_path(key: &'static str, value: &Path) -> Self {
        Self::set_value(key, value)
    }

    fn set_value(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let previous = std::env::var_os(key);
        // SAFETY: nextest runs each test in a separate process.
        unsafe { std::env::set_var(key, value) };
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => unsafe { std::env::set_var(self.key, value) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

async fn require_nats_server() -> Result<Option<common::NatsServerHandle>> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        eprintln!("skipping: nats-server not available");
        return Ok(None);
    };
    Ok(Some(server))
}

fn append_user_message_entry(message_id: &str, text: &str) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: Some(message_id.to_string()),
        role: MessageRole::User,
        content: MessageContent::Text(text.to_string()),
        timestamp: None,
        fence_token: None,
    }
}

fn user_message_texts(entries: &[(u64, SessionLogEntry)]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|(_, entry)| match entry {
            SessionLogEntry::Message { role, content, .. } if role.is_user() => {
                Some(content.to_text())
            }
            _ => None,
        })
        .collect()
}

fn counting_stub_call_fn(counter: Arc<AtomicUsize>) -> harnx_runtime::agent_loop::AgentCallFn {
    Arc::new(move |_input, _config, _abort| {
        let counter = Arc::clone(&counter);
        Box::pin(async move {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok((
                "done".to_string(),
                None,
                vec![],
                CompletionTokenUsage::default(),
            ))
        })
    })
}

async fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> Result<()> {
    tokio::time::timeout(timeout, async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .map_err(anyhow::Error::from)
}

#[path = "nats_worker/frontend_affine_workers.rs"]
mod tests;
