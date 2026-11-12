//! Telling opted-in waits that a new message arrived for their session.
//!
//! The signal ends a wait early and cancels nothing. A tool server answers
//! `Unknown` for a call it isn't running, which covers a call it hasn't
//! admitted yet as well as one that already finished, so the signal is resent
//! a bounded number of times. A message already pending when a call is
//! dispatched travels in the request instead, as
//! [`CAPABILITY_NEW_MESSAGE_PENDING`], since a control message sent then can
//! reach the server before it admits the call.
use super::*;
use harnx_toolset::CancelAcceptance;
use harnx_toolset_server::cancellation_client::request_cancellation;

/// How many times one signal is sent while the server keeps answering
/// `Unknown`, which is also what a missing or mismatched answer counts as.
pub(crate) const NEW_MESSAGE_ATTEMPTS: usize = 5;
const NEW_MESSAGE_RETRY_DELAY: Duration = Duration::from_millis(200);
const NEW_MESSAGE_REPLY_TIMEOUT: Duration = Duration::from_secs(1);

impl NatsInFlightCalls {
    /// Record that a user message landed at `seq` for the session, and tell
    /// every opted-in call in flight for it. Returns how many calls it
    /// signals. The sends, and their resends, run on their own tasks, so a
    /// caller following the session log is never held up by a tool server.
    pub fn note_pending_message(
        &self,
        client: &async_nats::Client,
        session_id: &str,
        seq: u64,
    ) -> usize {
        let targets = self.pending_message_targets(session_id, seq);
        for target in &targets {
            spawn_new_message_signal(client, target, session_id);
        }
        targets.len()
    }
}

impl NatsToolProvider {
    /// Whether a call to `route` should start with its new-message signal
    /// already fired, because a message is waiting for this session.
    pub(super) fn dispatch_with_message_pending(&self, route: &RegisteredTool) -> bool {
        route.stop_on_new_message
            && self
                .parent_session_id
                .as_deref()
                .is_some_and(|session| self.in_flight.message_pending(session))
    }

    /// Signal a just-registered call about a message that landed after its
    /// request was built, which therefore doesn't carry the capability. The
    /// session watcher noted that message before the call was registered,
    /// so it had no call to signal.
    pub(super) fn signal_message_since_dispatch(&self, request: &ToolRequest, server: &str) {
        let Some(session) = request.parent_session_id.as_deref() else {
            return;
        };
        if request
            .capabilities
            .contains(CAPABILITY_NEW_MESSAGE_PENDING)
            || !self.in_flight.message_pending(session)
        {
            return;
        }
        let target = InFlightCancelTarget {
            call_id: request.call_id.clone(),
            server: server.to_owned(),
            control_subject: self.instance_id.control_subject(),
        };
        spawn_new_message_signal(&self.client, &target, session);
    }
}

fn spawn_new_message_signal(
    client: &async_nats::Client,
    target: &InFlightCancelTarget,
    session_id: &str,
) {
    let control = ControlMessage::new_message(
        target.server.clone(),
        session_id.to_owned(),
        target.call_id.clone(),
    );
    let client = client.clone();
    let subject = target.control_subject.clone();
    tokio::spawn(async move { send_until_settled(&client, &subject, &control).await });
}

/// Resend while the server can't say whether the call is running, and stop
/// as soon as it accepts the signal, reports the call finished, or rejects it.
async fn send_until_settled(client: &async_nats::Client, subject: &str, control: &ControlMessage) {
    let call_id = &control.call_id;
    for attempt in 1..=NEW_MESSAGE_ATTEMPTS {
        if attempt > 1 {
            tokio::time::sleep(NEW_MESSAGE_RETRY_DELAY).await;
        }
        let acknowledgement = request_cancellation(
            client,
            subject.to_owned(),
            control,
            NEW_MESSAGE_REPLY_TIMEOUT,
        )
        .await;
        match acknowledgement.acceptance {
            CancelAcceptance::Unknown { reason } => {
                log::debug!("new-message signal unsettled: call_id={call_id} attempt={attempt} reason={reason}");
            }
            acceptance => {
                log::debug!("new-message signal settled: call_id={call_id} {acceptance:?}");
                return;
            }
        }
    }
    log::debug!(
        "new-message signal gave up after {NEW_MESSAGE_ATTEMPTS} attempts: call_id={call_id}"
    );
}
