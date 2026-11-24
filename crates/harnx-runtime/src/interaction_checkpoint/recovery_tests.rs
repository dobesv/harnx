use super::{classify, CallRecovery, CheckpointStore, ResultSnapshot, RoundRecovery};
use crate::interaction_checkpoint::snapshot::CallCheckpoint;
use crate::{
    config::GlobalConfig,
    nats_session_log::NatsSessionLog,
    nats_session_metadata::{SessionInitializer, SessionMetadata, SessionMetadataStore},
    nats_worker::{tests::spawn_test_nats, NatsSessionLogBackend},
};
use anyhow::{Context, Result};
use async_nats::jetstream;
use harnx_core::{
    session::SessionLogEntry,
    tool::{ToolCall, ToolResult},
};
use harnx_engine::tool::{InnerToolCallStatus, MaterializedToolResult, ToolCallCheckpoint};
use serde_json::json;

struct Fixture {
    storage_key: String,
    store: CheckpointStore,
    backend: NatsSessionLogBackend,
    config: GlobalConfig,
    js: jetstream::Context,
    round: u64,
    call: ToolCall,
    prefix: String,
    _broker: crate::nats_worker::tests::TestNatsServer,
    _dir: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> Result<Self> {
        let (url, _broker, _dir) = spawn_test_nats()
            .await
            .context("NATS test binary is required")?;
        let client = async_nats::connect(&url).await?;
        let js = jetstream::new(client);

        let metadata_store = SessionMetadataStore::ensure(&js, 1).await?;
        let local_id = crate::nats_worker::new_remote_session_id();
        metadata_store
            .create(&SessionMetadata::new(
                &local_id,
                SessionInitializer::named("test_agent", Default::default()),
            ))
            .await?;
        let storage_key = harnx_core::session_identity::session_key(Some("test_agent"), &local_id);

        NatsSessionLog::new_with_replicas(js.clone(), storage_key.clone(), 1)
            .append_event_async(&SessionLogEntry::Clear)
            .await?;

        let store = CheckpointStore::ensure(&js, 1).await?;
        let backend = NatsSessionLogBackend::new(js.clone(), storage_key.clone(), 1);
        let config = GlobalConfig::default();
        let round = 1;
        let call = ToolCall::new(
            "bash_exec".into(),
            json!({"cmd": "echo 1"}),
            Some("c1".into()),
            None,
        );
        let prefix = super::records::call_prefix(&storage_key, round, "c1");

        Ok(Self {
            storage_key,
            store,
            backend,
            config,
            js,
            round,
            call,
            prefix,
            _broker,
            _dir,
        })
    }

    fn chain_version(&self) -> Result<String> {
        Ok(self.config.read().resolved_middleware()?.version)
    }

    fn missing_checkpoint_ref(&self) -> String {
        format!("{}checkpoints.nonexistent", self.prefix)
    }

    async fn start_round(&self) -> Result<()> {
        self.store
            .start_round(&super::super::RoundStart {
                version: 1,
                session_storage_key: self.storage_key.clone(),
                tool_round_seq: self.round,
                fence_token: 1,
            })
            .await
    }

    async fn put_attempt(
        &self,
        attempt_id: &str,
        checkpoint: Option<String>,
    ) -> Result<super::records::AttemptStart> {
        let attempt = super::records::AttemptStart {
            version: 1,
            session_storage_key: self.storage_key.clone(),
            tool_round_seq: self.round,
            tool_call_id: "c1".into(),
            attempt_id: attempt_id.into(),
            chain_version: self.chain_version()?,
            checkpoint,
            fence_token: 1,
        };
        self.store
            .put(
                &self.prefix,
                &format!("attempts.{attempt_id}.start"),
                &attempt,
            )
            .await?;
        Ok(attempt)
    }

    async fn put_dispatch(
        &self,
        attempt_id: &str,
        physical_id: &str,
        completed_snapshot: Option<&str>,
    ) -> Result<()> {
        let intent = super::records::DispatchIntent {
            version: 1,
            attempt_id: attempt_id.into(),
            physical_invocation_id: physical_id.into(),
            kind: super::records::DispatchKind::Next,
            step_path: vec![],
            call: self.call.clone(),
            fence_token: 1,
        };
        self.store
            .put(
                &self.prefix,
                &format!("attempts.{attempt_id}.dispatches.{physical_id}.intent"),
                &intent,
            )
            .await?;
        if let Some(snapshot) = completed_snapshot {
            let completed = super::records::DispatchCompleted {
                version: 1,
                physical_invocation_id: physical_id.into(),
                snapshot: snapshot.into(),
                fence_token: 1,
            };
            self.store
                .put(
                    &self.prefix,
                    &format!("attempts.{attempt_id}.dispatches.{physical_id}.completed"),
                    &completed,
                )
                .await?;
        }
        Ok(())
    }

    async fn setup_attempt_with_dispatch(&self, snapshot: Option<&str>) -> Result<RoundRecovery> {
        self.start_round().await?;
        self.put_attempt("att-1", None).await?;
        self.put_dispatch("att-1", "phys-1", snapshot).await?;
        self.classify().await
    }

    async fn put_checkpoint(&self, suffix: &str) -> Result<String> {
        let cp = CallCheckpoint::new(&ToolCallCheckpoint {
            state: json!({"version": 1}),
            snapshots: Default::default(),
        });
        self.store.put(&self.prefix, suffix, &cp).await
    }

    async fn append_completed(
        &self,
        public: harnx_core::session::ToolOutput,
        snapshot_ref: String,
    ) -> Result<u64> {
        self.backend
            .append_event(&SessionLogEntry::ToolCallCompleted {
                tool_round_seq: self.round,
                tool_call_id: "c1".into(),
                result: public,
                private_snapshot: snapshot_ref,
                fence_token: 1,
            })
            .await
    }

    async fn complete(&self, result: &ToolResult, suffix: &str) -> Result<String> {
        let snapshot = ResultSnapshot::new(&MaterializedToolResult::new(
            result.clone(),
            InnerToolCallStatus::Ok,
        ));
        let snapshot_ref = self.store.put(&self.prefix, suffix, &snapshot).await?;
        self.append_completed(super::public_output(result), snapshot_ref.clone())
            .await?;
        Ok(snapshot_ref)
    }

    async fn append_request(&self, checkpoint: String) -> Result<u64> {
        let chain_version = self.chain_version()?;
        self.backend
            .append_event(&SessionLogEntry::InteractionRequested {
                tool_round_seq: self.round,
                tool_call_id: "c1".into(),
                interaction_id: "int-1".into(),
                producer: "middleware:test".into(),
                step_path: vec![],
                key: "confirm".into(),
                kind: "confirm".into(),
                payload: json!({}),
                summary: "summary".into(),
                chain_version,
                checkpoint,
                fence_token: 1,
            })
            .await
    }

    async fn setup_no_roundstart_middleware(&self) -> Result<()> {
        self.js
            .delete_key_value(super::super::ROUND_START_BUCKET)
            .await?;
        self.config.write().middleware = serde_json::from_value(json!([
            {"name": "test_mw", "match": "^matched_.*", "bash": "next"}
        ]))?;
        Ok(())
    }

    async fn check_no_roundstart(&self, tool: &str, expected: CallRecovery) -> Result<()> {
        let call = ToolCall::new(tool.into(), json!({}), Some("c1".into()), None);
        self.setup_no_roundstart_middleware().await?;
        let recovery = self.classify_calls(&[call]).await?;
        assert!(!recovery.owned);
        assert_eq!(recovery.calls, vec![expected]);
        Ok(())
    }

    async fn classify(&self) -> Result<RoundRecovery> {
        classify(
            &self.config,
            &self.backend,
            self.round,
            std::slice::from_ref(&self.call),
        )
        .await
    }

    async fn classify_calls(&self, calls: &[ToolCall]) -> Result<RoundRecovery> {
        classify(&self.config, &self.backend, self.round, calls).await
    }

    fn assert_missing_checkpoint_interrupted(recovery: &RoundRecovery) {
        assert!(recovery.owned);
        assert!(!recovery.started);
        assert_eq!(recovery.calls.len(), 1);
        assert!(matches!(
            &recovery.calls[0],
            CallRecovery::Interrupted(msg) if msg.contains("missing interaction checkpoint data")
        ));
    }
}

#[tokio::test]
async fn completed_call_restores_snapshot_and_matches_identity() -> Result<()> {
    let f = Fixture::new().await?;
    let result = ToolResult::new(f.call.clone(), json!({"output": "hello"}));
    f.complete(&result, "completions.1").await?;

    let recovery = f.classify().await?;
    assert!(recovery.owned);
    assert_eq!(recovery.calls.len(), 1);
    assert_eq!(recovery.calls[0], CallRecovery::Completed(Box::new(result)));
    Ok(())
}

#[tokio::test]
async fn completed_call_missing_snapshot_becomes_interrupted() -> Result<()> {
    let f = Fixture::new().await?;
    let missing_ref = format!("{}completions.nonexistent", f.prefix);
    let result = ToolResult::new(f.call.clone(), json!({"output": "hello"}));
    f.append_completed(super::public_output(&result), missing_ref)
        .await?;

    let recovery = f.classify().await?;
    assert!(recovery.owned);
    assert_eq!(recovery.calls.len(), 1);
    assert!(matches!(
        &recovery.calls[0],
        CallRecovery::Interrupted(msg) if msg.contains("missing interaction checkpoint")
    ));
    Ok(())
}

#[tokio::test]
async fn completed_call_identity_mismatch_becomes_interrupted() -> Result<()> {
    let f = Fixture::new().await?;
    let other_call = ToolCall::new("other_tool".into(), json!({}), Some("c1".into()), None);
    let result = ToolResult::new(other_call, json!({"output": "hello"}));
    f.complete(&result, "completions.1").await?;

    let recovery = f.classify().await?;
    assert!(recovery.owned);
    assert_eq!(recovery.calls.len(), 1);
    assert!(matches!(
        &recovery.calls[0],
        CallRecovery::Interrupted(msg) if msg.contains("identity mismatch")
    ));
    Ok(())
}

#[tokio::test]
async fn roundstart_confirmed_no_attempts_becomes_legacy() -> Result<()> {
    let f = Fixture::new().await?;
    f.start_round().await?;

    let recovery = f.classify().await?;
    assert!(recovery.owned);
    assert_eq!(recovery.calls, vec![CallRecovery::Legacy]);
    Ok(())
}

#[tokio::test]
async fn no_roundstart_no_attempts_matching_middleware_becomes_interrupted() -> Result<()> {
    let f = Fixture::new().await?;
    f.check_no_roundstart(
        "matched_tool",
        CallRecovery::Interrupted(
            "interrupted, outcome unknown: pre-interaction round with matching middleware"
                .to_string(),
        ),
    )
    .await
}

#[tokio::test]
async fn no_roundstart_no_attempts_without_matching_middleware_becomes_legacy() -> Result<()> {
    let f = Fixture::new().await?;
    f.check_no_roundstart("unmatched_tool", CallRecovery::Legacy)
        .await
}

#[tokio::test]
async fn attempt_with_no_intent_becomes_resume() -> Result<()> {
    let f = Fixture::new().await?;
    f.start_round().await?;
    f.put_attempt("att-1", None).await?;

    let recovery = f.classify().await?;
    assert!(recovery.owned);
    assert_eq!(recovery.calls, vec![CallRecovery::Resume]);
    Ok(())
}

#[tokio::test]
async fn attempt_with_intent_and_downstream_completed_becomes_interrupted() -> Result<()> {
    let f = Fixture::new().await?;
    let recovery = f
        .setup_attempt_with_dispatch(Some("snapshot-dummy"))
        .await?;
    assert!(recovery.owned);
    assert_eq!(
        recovery.calls,
        vec![CallRecovery::Interrupted(
            "interrupted, outcome unknown: the downstream tool completed but the middleware didn't finish".to_string()
        )]
    );
    Ok(())
}

#[tokio::test]
async fn attempt_with_intent_and_downstream_unknown_becomes_interrupted() -> Result<()> {
    let f = Fixture::new().await?;
    let recovery = f.setup_attempt_with_dispatch(None).await?;
    assert!(recovery.owned);
    assert_eq!(
        recovery.calls,
        vec![CallRecovery::Interrupted(
            "interrupted, outcome unknown: the downstream outcome is unknown".to_string()
        )]
    );
    Ok(())
}

#[tokio::test]
async fn valid_request_boundary_pending_or_answered_becomes_resume() -> Result<()> {
    let f = Fixture::new().await?;
    f.start_round().await?;
    f.put_attempt("att-1", None).await?;
    let cp_ref = f.put_checkpoint("checkpoints.1").await?;
    f.append_request(cp_ref).await?;

    let recovery = f.classify().await?;
    assert!(recovery.owned);
    assert_eq!(recovery.calls, vec![CallRecovery::Resume]);
    Ok(())
}

#[tokio::test]
async fn crashed_rerun_after_answer_with_intent_applies_intents() -> Result<()> {
    let f = Fixture::new().await?;
    f.start_round().await?;
    let cp_ref = f.put_checkpoint("checkpoints.1").await?;
    f.append_request(cp_ref.clone()).await?;

    f.put_attempt("att-2", Some(cp_ref)).await?;
    f.put_dispatch("att-2", "phys-2", Some("snapshot-dummy"))
        .await?;

    let recovery = f.classify().await?;
    assert!(recovery.owned);
    assert_eq!(
        recovery.calls,
        vec![CallRecovery::Interrupted(
            "interrupted, outcome unknown: the downstream tool completed but the middleware didn't finish".to_string()
        )]
    );
    Ok(())
}

#[tokio::test]
async fn attempts_ordered_by_kv_revision_not_uuid() -> Result<()> {
    let f = Fixture::new().await?;
    f.start_round().await?;

    f.put_attempt("z-uuid", None).await?;
    f.put_dispatch("z-uuid", "phys-z", Some("snapshot-dummy"))
        .await?;

    f.put_attempt("a-uuid", None).await?;

    let recovery = f.classify().await?;
    assert!(recovery.owned);
    assert_eq!(recovery.calls, vec![CallRecovery::Resume]);
    Ok(())
}

#[tokio::test]
async fn request_with_missing_checkpoint_and_no_attempt_is_interrupted() -> Result<()> {
    let f = Fixture::new().await?;
    f.append_request(f.missing_checkpoint_ref()).await?;

    let recovery = f.classify().await?;
    Fixture::assert_missing_checkpoint_interrupted(&recovery);
    Ok(())
}

#[tokio::test]
async fn attempt_with_missing_checkpoint_is_interrupted() -> Result<()> {
    let f = Fixture::new().await?;
    f.put_attempt("att-1", Some(f.missing_checkpoint_ref()))
        .await?;

    let recovery = f.classify().await?;
    Fixture::assert_missing_checkpoint_interrupted(&recovery);
    Ok(())
}

#[tokio::test]
async fn attempt_with_mismatched_identity_is_interrupted() -> Result<()> {
    let f = Fixture::new().await?;
    let attempt = super::records::AttemptStart {
        version: 1,
        session_storage_key: f.storage_key.clone(),
        tool_round_seq: f.round,
        tool_call_id: "other_id".into(),
        attempt_id: "att-1".into(),
        chain_version: f.chain_version()?,
        checkpoint: None,
        fence_token: 1,
    };
    f.store
        .put(&f.prefix, "attempts.att-1.start", &attempt)
        .await?;

    let recovery = f.classify().await?;
    assert_eq!(recovery.calls.len(), 1);
    assert!(matches!(
        &recovery.calls[0],
        CallRecovery::Interrupted(msg) if msg.contains("attempt identity mismatch")
    ));
    Ok(())
}

#[tokio::test]
async fn suspension_ready_alone_without_call_records_has_legacy_slots() -> Result<()> {
    let f = Fixture::new().await?;
    f.backend
        .append_event(&SessionLogEntry::SuspensionReady {
            tool_round_seq: f.round,
            pending: vec!["int-1".into()],
            fence_token: 1,
        })
        .await?;

    let recovery = f.classify().await?;
    assert!(!recovery.owned);
    assert!(!recovery.started);
    assert_eq!(recovery.calls, vec![CallRecovery::Legacy]);
    Ok(())
}

#[tokio::test]
async fn round_start_only_with_two_calls_is_owned_legacy() -> Result<()> {
    let f = Fixture::new().await?;
    let call2 = ToolCall::new("bash_exec".into(), json!({}), Some("c2".into()), None);
    f.start_round().await?;

    let recovery = f.classify_calls(&[f.call.clone(), call2]).await?;
    assert!(recovery.owned);
    assert!(recovery.started);
    assert_eq!(
        recovery.calls,
        vec![CallRecovery::Legacy, CallRecovery::Legacy]
    );
    Ok(())
}

#[tokio::test]
async fn pre_t2_2_mixed_call_completed_and_unstarted_legacy_preserves_started_false() -> Result<()>
{
    let f = Fixture::new().await?;
    let call2 = ToolCall::new(
        "non_middleware_tool".into(),
        json!({}),
        Some("c2".into()),
        None,
    );

    let res1 = ToolResult::new(f.call.clone(), json!({"output": "c1_done"}));
    f.complete(&res1, "completions.1").await?;

    let recovery = f.classify_calls(&[f.call.clone(), call2]).await?;
    assert!(recovery.owned);
    assert!(!recovery.started);
    assert_eq!(
        recovery.calls,
        vec![
            CallRecovery::Completed(Box::new(res1)),
            CallRecovery::Legacy
        ]
    );
    Ok(())
}
