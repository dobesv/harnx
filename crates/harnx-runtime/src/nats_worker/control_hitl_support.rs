use super::*;
use crate::nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig};
use crate::nats_worker::tests::spawn_test_nats;
use anyhow::{Context, Result};
use harnx_core::tool::ToolCall;
use serde_json::json;
use std::sync::Arc;

pub(super) struct HitlControlFixture {
    pub handler: SessionControlHandler,
    pub backend: NatsSessionLogBackend,
    pub decisions: tokio::sync::mpsc::UnboundedReceiver<AppliedHitlDecision>,
    pub lease: Arc<NatsSessionLease>,
    pub calls: SessionLogEntry,
    pub request: SessionLogEntry,
    pub _server: crate::nats_worker::tests::TestNatsServer,
    pub _dir: tempfile::TempDir,
}

impl HitlControlFixture {
    pub async fn start() -> Result<Self> {
        harnx_core::require_nextest();
        let (url, server, dir) = spawn_test_nats().await.context("nats-server required")?;
        let client = async_nats::connect(url).await?;
        let js = async_nats::jetstream::new(client.clone());
        let session_id = "hitl-control-fence";
        let lease = Arc::new(
            NatsSessionLease::acquire(NatsLeaseAcquireParams {
                jetstream: js.clone(),
                session_id,
                worker_id: "hitl-worker".into(),
                generation: 1,
                config: NatsLeaseConfig::default(),
                session_metadata: None,
            })
            .await?
            .context("worker lease")?,
        );
        let backend = NatsSessionLogBackend::new(js.clone(), session_id, 1);
        let (tx, decisions) = tokio::sync::mpsc::unbounded_channel();
        let handler = SessionControlHandler {
            client,
            jetstream: js,
            session_id: session_id.into(),
            lease: lease.clone(),
            backend: backend.clone(),
            abort_signal: crate::utils::create_abort_signal(),
            hitl_decision_tx: tx,
        };
        let calls = SessionLogEntry::ToolCalls {
            text: String::new(),
            thought: None,
            calls: vec![ToolCall::new(
                "reused_tool".into(),
                json!({}),
                Some("reused".into()),
                None,
            )],
            timestamp: None,
            fence_token: Some(lease.fence_token()),
        };
        let request = SessionLogEntry::HitlApprovalRequested {
            tool_call_id: "reused".into(),
            summary: "Approve read".into(),
            fence_token: lease.fence_token(),
        };
        Ok(Self {
            handler,
            backend,
            decisions,
            lease,
            calls,
            request,
            _server: server,
            _dir: dir,
        })
    }
}

pub(super) async fn verify_same_round_replacement(fixture: &mut HitlControlFixture) -> Result<u64> {
    fixture.backend.append_event(&fixture.calls).await?;
    let original = fixture.backend.append_event(&fixture.request).await?;
    // A replaced request in the SAME round must not adopt the old answer.
    let replacement = fixture.backend.append_event(&fixture.request).await?;
    fixture
        .handler
        .apply_hitl_decision(
            HitlDecisionParams::new("reused", true, None, Some(original)),
            None,
        )
        .await;
    let tail = fixture.backend.last_entry_async().await?.unwrap().0;
    assert_eq!(tail, replacement);
    assert!(fixture.decisions.try_recv().is_err());
    // Only the replacement generation is eligible for the fenced append.
    fixture
        .handler
        .apply_hitl_decision(
            HitlDecisionParams::new("reused", true, None, Some(replacement)),
            None,
        )
        .await;
    assert!(fixture.decisions.try_recv().is_ok());
    Ok(replacement)
}

pub(super) async fn verify_reused_id_next_round(
    fixture: &mut HitlControlFixture,
    replacement: u64,
) -> Result<()> {
    fixture
        .backend
        .append_event(&SessionLogEntry::ToolResults {
            results: vec![],
            timestamp: None,
        })
        .await?;
    fixture.backend.append_event(&fixture.calls).await?;
    let next = fixture.backend.append_event(&fixture.request).await?;
    fixture
        .handler
        .apply_hitl_decision(
            HitlDecisionParams::new("reused", false, None, Some(replacement)),
            None,
        )
        .await;
    fixture
        .handler
        .apply_hitl_decision(
            HitlDecisionParams::new("wrong-call", false, None, Some(next)),
            None,
        )
        .await;
    assert_eq!(fixture.backend.last_entry_async().await?.unwrap().0, next);
    assert!(fixture.decisions.try_recv().is_err());
    fixture
        .handler
        .apply_hitl_decision(
            HitlDecisionParams::new("reused", false, Some("fresh".into()), Some(next)),
            None,
        )
        .await;
    assert_eq!(
        fixture.decisions.try_recv().unwrap().note.as_deref(),
        Some("fresh")
    );
    Ok(())
}

pub(super) async fn verify_terminal_precedence_at_append(
    fixture: &mut HitlControlFixture,
) -> Result<()> {
    fixture
        .backend
        .append_event(&SessionLogEntry::ToolResults {
            results: vec![],
            timestamp: None,
        })
        .await?;
    fixture.backend.append_event(&fixture.calls).await?;
    let pending = fixture.backend.append_event(&fixture.request).await?;
    let terminal = fixture
        .backend
        .append_event(&SessionLogEntry::TurnEnd {
            through_seq: 1,
            fence_token: fixture.lease.fence_token(),
            timestamp: None,
            usage: None,
        })
        .await?;
    fixture
        .handler
        .apply_hitl_decision(
            HitlDecisionParams::new("reused", true, None, Some(pending)),
            None,
        )
        .await;
    assert_eq!(
        fixture.backend.last_entry_async().await?.unwrap().0,
        terminal
    );
    assert!(fixture.decisions.try_recv().is_err());
    fixture.lease.release().await?;
    Ok(())
}
