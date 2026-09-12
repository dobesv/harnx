//! Acknowledged replies for Core NATS RPC. Retransmit results, never handlers.
//!
//! Opt-in headers keep ordinary NATS request clients compatible. A caller
//! retains one inbox through reconnect; a server retains its finished result
//! until receipt is acknowledged or the recovery deadline expires.

use crate::recovery::RECOVERY_TIMEOUT;
use anyhow::{Context, Result};
use async_nats::{Client, Message, Request, Subject};
use futures_util::StreamExt;
use std::time::Duration;

const RELIABLE_REPLY: &str = "Harnx-Acknowledge-Reply";
const ACTIVITY_INBOX: &str = "Harnx-Request-Activity";

pub async fn request(client: &Client, subject: String, mut request: Request) -> Result<Message> {
    request
        .headers
        .get_or_insert_default()
        .insert(RELIABLE_REPLY, "1");
    let inbox = client.new_inbox();
    let mut activity = tokio::time::timeout(RECOVERY_TIMEOUT, client.subscribe(inbox.clone()))
        .await
        .context("RPC activity subscription timed out")??;
    request
        .headers
        .get_or_insert_default()
        .insert(ACTIVITY_INBOX, inbox);
    let response = client.send_request(subject, request);
    tokio::pin!(response);
    let deadline = tokio::time::sleep(RECOVERY_TIMEOUT);
    tokio::pin!(deadline);
    let message = loop {
        tokio::select! {
            result = &mut response => break result?,
            pulse = activity.next() => {
                pulse.context("RPC activity subscription closed; completion unconfirmed")?;
                deadline.as_mut().reset(tokio::time::Instant::now() + RECOVERY_TIMEOUT);
            }
            _ = &mut deadline => anyhow::bail!("RPC completion unconfirmed: request handler stopped acknowledging activity"),
        }
    };
    if let Some(ack) = message.reply.clone() {
        // The caller already has the result. A lost receipt only makes the
        // responder retransmit; it must not turn a known result into failure.
        // Callers may drop their last client as soon as this future returns.
        // Bound enqueueing too: a disconnected client's command queue can fill.
        let _ = tokio::time::timeout(Duration::from_secs(1), async {
            client.publish(ack, "received".into()).await?;
            client.flush().await.map_err(anyhow::Error::from)
        })
        .await;
    }
    Ok(message)
}

/// Retained by the request task, never by the handler itself. This confirms
/// delivery and process liveness without reissuing possibly mutating work.
/// Dropping/aborting a request also stops its heartbeats.
pub struct RequestActivity(Option<tokio::task::JoinHandle<()>>);

impl RequestActivity {
    pub fn start(client: &Client, message: &Message) -> Self {
        let inbox = message
            .headers
            .as_ref()
            .and_then(|headers| headers.get(ACTIVITY_INBOX));
        let task = inbox.map(|inbox| {
            let inbox = inbox.to_string();
            let client = client.clone();
            tokio::spawn(async move {
                let mut pulse = tokio::time::interval(Duration::from_secs(1));
                loop {
                    pulse.tick().await;
                    if client
                        .publish(inbox.clone(), "active".into())
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            })
        });
        Self(task)
    }
}

impl Drop for RequestActivity {
    fn drop(&mut self) {
        if let Some(task) = &self.0 {
            task.abort();
        }
    }
}

#[derive(Clone)]
pub struct ReplyTarget {
    subject: Subject,
    acknowledge: bool,
}

impl ReplyTarget {
    pub fn from_message(message: &Message) -> Result<Self> {
        Ok(Self {
            subject: message
                .reply
                .clone()
                .context("RPC request has no reply subject")?,
            acknowledge: message.headers.as_ref().is_some_and(|headers| {
                headers
                    .get(RELIABLE_REPLY)
                    .is_some_and(|value| value.as_str() == "1")
            }),
        })
    }

    pub async fn send(&self, client: &Client, payload: impl Into<bytes::Bytes>) -> Result<()> {
        let payload = payload.into();
        if !self.acknowledge {
            return client
                .publish(self.subject.clone(), payload)
                .await
                .context("publish RPC reply");
        }
        let inbox = client.new_inbox();
        tokio::time::timeout(RECOVERY_TIMEOUT, async {
            let mut receipts = client.subscribe(inbox.clone()).await?;
            let mut retry = tokio::time::interval(Duration::from_millis(250));
            loop {
                tokio::select! {
                    receipt = receipts.next() => {
                        let receipt = receipt.context("RPC reply receipt subscription closed")?;
                        // NATS can send 503/no-responders to this inbox while
                        // the caller is reconnecting. That is not a receipt.
                        if receipt.status.is_none() && receipt.payload.as_ref() == b"received" {
                            return Ok(());
                        }
                    }
                    _ = retry.tick() => {
                        client.publish_with_reply(self.subject.clone(), inbox.clone(), payload.clone()).await?;
                    }
                }
            }
        }).await.context("RPC reply delivery unconfirmed after reconnect deadline")?
    }
}
