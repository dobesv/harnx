//! Keep broker election running while frontends are idle or awaiting a turn.

use super::{ensure_shared_server, SharedNatsServer};
use anyhow::Result;
use std::time::Duration;
use tokio::{sync::watch, task::JoinHandle};

#[derive(Clone)]
pub struct LocalBrokerStatus {
    pub url: String,
    pub token: String,
    pub nonce: String,
    owner: bool,
}

impl LocalBrokerStatus {
    pub fn is_owner(&self) -> bool {
        self.owner
    }

    fn snapshot(server: &SharedNatsServer) -> Self {
        Self {
            url: server.url.clone(),
            token: server.token.clone(),
            nonce: server.nonce.clone(),
            owner: server.is_owner(),
        }
    }
}

/// One guard per frontend connection scope. The task retains broker ownership
/// and continuously participates in file-lock election. All endpoint users,
/// including subprocesses handed its URL/token, reconnect to the same broker
/// address without replacing clients, subscriptions, or running workers.
pub struct LocalBroker {
    status: watch::Receiver<LocalBrokerStatus>,
    task: JoinHandle<()>,
}

impl LocalBroker {
    pub async fn start() -> Result<Self> {
        let mut server = ensure_shared_server().await?;
        let (status, receiver) = watch::channel(LocalBrokerStatus::snapshot(&server));
        let task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                match server.refresh_if_stale().await {
                    Ok(true) => {
                        log::info!("shared local NATS recovered: nonce={}", server.nonce);
                        status.send_replace(LocalBrokerStatus::snapshot(&server));
                    }
                    Ok(false) => {}
                    Err(error) => {
                        log::warn!("shared local NATS recovery failed; retrying: {error:#}")
                    }
                }
            }
        });
        Ok(Self {
            status: receiver,
            task,
        })
    }

    pub fn status(&self) -> LocalBrokerStatus {
        self.status.borrow().clone()
    }

    pub fn is_running(&self) -> bool {
        !self.task.is_finished()
    }
}

impl Drop for LocalBroker {
    fn drop(&mut self) {
        self.task.abort();
    }
}
