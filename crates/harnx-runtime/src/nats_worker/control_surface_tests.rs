//! Worker-side surface action control tests (review N-B): a worker that
//! cannot prove lease ownership must refuse the attempt promptly instead of
//! acknowledging `Pending`, and a panicking handler must still answer the
//! client rather than leaving it to time out.

use super::*;

// The HITL control fixture already owns the expensive scaffold (NATS
// server, lease, log backend, handler); reusing it keeps the two control
// test sets from drifting apart.
use super::hitl_tests::control_hitl_support::HitlControlFixture;
use crate::surfaces::{SurfaceActionOutcome, SurfaceActionReply, SurfaceActionSubmission};

use anyhow::{Context, Result};
use futures_util::StreamExt;
use std::time::Duration;

/// The surface action command side of the control fixture: a reply inbox
/// that observes exactly what the worker publishes.
struct SurfaceCommandFixture {
    support: HitlControlFixture,
    inbox: async_nats::Subject,
    replies: async_nats::Subscriber,
}

impl SurfaceCommandFixture {
    async fn start() -> Result<Self> {
        let support = HitlControlFixture::start().await?;
        let inbox = support.handler.client.new_inbox();
        let replies = support.handler.client.subscribe(inbox.clone()).await?;
        Ok(Self {
            support,
            inbox: inbox.into(),
            replies,
        })
    }

    async fn entry_count(&self) -> Result<usize> {
        Ok(self.support.backend.load_events_latest_async().await?.len())
    }
}

fn test_submission() -> SurfaceActionSubmission {
    SurfaceActionSubmission {
        surface_id: "surface-1".into(),
        publish_seq: 7,
        response: harnx_core::interaction::InteractionResponse {
            action: "note_act".into(),
            data: serde_json::from_value(serde_json::json!({})).unwrap(),
            note: None,
        },
        user_message: None,
    }
}

async fn next_reply(replies: &mut async_nats::Subscriber) -> Result<SurfaceActionReply> {
    let message = tokio::time::timeout(Duration::from_secs(10), replies.next())
        .await
        .context("reply missing")?
        .context("reply subscription closed")?;
    Ok(serde_json::from_slice(&message.payload)?)
}

async fn no_more_replies(replies: &mut async_nats::Subscriber) -> bool {
    tokio::time::timeout(Duration::from_millis(300), replies.next())
        .await
        .is_err()
}

/// N-B: a worker that cannot prove lease ownership must refuse the surface
/// action promptly — `lease_lost`, never a `Pending` ack — and must run and
/// append nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn surface_action_non_holder_refuses_instead_of_acking_pending() -> Result<()> {
    let mut fixture = SurfaceCommandFixture::start().await?;
    // Hand the lease away: this worker is now a stale subscriber that
    // cannot prove ownership.
    fixture.support.lease.release().await?;
    assert!(
        !fixture.support.handler.holds_session_lease().await,
        "fixture must not hold the lease after release"
    );

    let command = ControlCommand::SurfaceAction {
        submission: test_submission(),
    };
    let entries_before = fixture.entry_count().await?;
    fixture
        .support
        .handler
        .apply(command, Some(fixture.inbox.clone()))
        .await;

    let reply = next_reply(&mut fixture.replies).await?;
    assert_eq!(
        reply.outcome,
        SurfaceActionOutcome::LeaseLost,
        "non-holder must refuse promptly"
    );
    assert!(
        no_more_replies(&mut fixture.replies).await,
        "the refusal must be the only answer — no Pending, no record"
    );
    assert_eq!(
        fixture.entry_count().await?,
        entries_before,
        "a non-holder must not append"
    );
    Ok(())
}

/// N-B: a handler panic after the early ack must still answer the client —
/// the nothing-recorded outcome instead of silence the client would wait
/// out for its whole acked-outcome budget.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn surface_action_panic_is_contained_and_answered() -> Result<()> {
    let mut fixture = SurfaceCommandFixture::start().await?;
    let entries_before = fixture.entry_count().await?;
    fixture
        .support
        .handler
        .answer_if_panicked(Some(fixture.inbox.clone()), async {
            panic!("surface action handler exploded");
        })
        .await;

    let reply = next_reply(&mut fixture.replies).await?;
    assert_eq!(
        reply.outcome,
        SurfaceActionOutcome::AppendFailed,
        "a contained panic must answer with the nothing-recorded outcome"
    );
    assert!(reply.turn_text.is_none());
    assert_eq!(
        fixture.entry_count().await?,
        entries_before,
        "a panicked handler must not have appended"
    );
    Ok(())
}
