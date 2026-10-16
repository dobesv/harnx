use super::*;
use crate::config::{Config, ConfigLock};
use crate::nats_session_metadata::{
    CallTimeoutOverride, InvocationEdgeKind, RunIdentity, RunLimitsPolicySource,
};
use chrono::Duration;
use harnx_core::timing_identity::{MilestoneCoverage, TIMING_HEADER};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex,
};

#[derive(Default)]
struct Log {
    entries: Mutex<Vec<(u64, SessionLogEntry)>>,
    fail_after_append: AtomicBool,
}

impl SessionAppendSink for Log {
    fn append(&self, entry: &SessionLogEntry) -> Result<u64> {
        let mut entries = self.entries.lock().unwrap();
        let seq = entries.len() as u64 + 1;
        entries.push((seq, entry.clone()));
        if self.fail_after_append.swap(false, Ordering::SeqCst) {
            anyhow::bail!("crash after durable append before memory update");
        }
        Ok(seq)
    }
}

fn origin() -> DateTime<Utc> {
    "2026-01-15T12:00:00Z".parse().unwrap()
}

fn limits() -> RunLimitsRecord {
    RunLimitsRecord {
        run_id: RunIdentity::from_string("root".into()),
        invocation_id: InvocationIdentity::from_string("invocation".into()),
        admitted_at: origin(),
        deadline: Some(origin() + Duration::seconds(3600)),
        policy_source: RunLimitsPolicySource::GlobalDefault,
        parent_invocation: None,
    }
}

fn config(log: &Arc<Log>, limits: Option<RunLimitsRecord>) -> GlobalConfig {
    let mut config = Config::default();
    let mut session = crate::config::session::new(&config, "timing-recovery", None).unwrap();
    session.runtime = Some(Arc::new(log.clone() as Arc<dyn SessionAppendSink>));
    config.session = Some(session);
    config.run_context = limits;
    Arc::new(ConfigLock::new(config))
}

fn prepare(config: &GlobalConfig, at: DateTime<Utc>) -> Result<Input> {
    let mut input = crate::config::input::from_str(config, "prompt", None);
    input.skip_user_log_append = true;
    prepare_notice(config, &mut input, at)?;
    Ok(input)
}

fn entry(receipt: &TimingReceipt) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: Some(format!(
            "timing-v1-{}",
            harnx_core::crypto::sha256(&receipt.to_json())
        )),
        role: MessageRole::User,
        content: MessageContent::Text(format!(
            "{TIMING_HEADER}{} | ignored prose",
            receipt.to_json()
        )),
        timestamp: None,
        fence_token: None,
    }
}

#[test]
fn timing_recovery_after_append_before_memory_commit_suppresses_covered_milestones() -> Result<()> {
    let log = Arc::new(Log::default());
    let config = config(&log, Some(limits()));
    start_run(&config, origin());
    log.fail_after_append.store(true, Ordering::SeqCst);
    let at = origin() + Duration::seconds(2800);
    assert!(prepare(&config, at).is_err());
    assert!(config
        .read()
        .timing_run
        .as_ref()
        .unwrap()
        .state
        .last_receipt
        .is_none());
    assert!(config.read().session.as_ref().unwrap().messages.is_empty());
    let receipt = recover_progress(&log.entries.lock().unwrap(), Some(&limits())).unwrap();
    assert_eq!(receipt.highest_elapsed_bucket, 3);
    assert!(receipt.coverage.contains(MilestoneCoverage::PERCENT_75));
    assert!(!receipt.coverage.contains(MilestoneCoverage::PERCENT_90));

    // A new config/sink models process loss, not merely clearing local progress.
    let restarted = self::config(&log, Some(limits()));
    hydrate_run(&restarted, Some(receipt.clone()));
    start_run(&restarted, at);
    assert!(prepare(&restarted, at)?.timing_context.is_none());
    assert_eq!(log.entries.lock().unwrap().len(), 1);
    let next = prepare(&restarted, origin() + Duration::seconds(3240))?;
    let next = &next.timing_context.unwrap().receipt;
    assert!(next.coverage.contains(MilestoneCoverage::PERCENT_90));
    assert_eq!(next.highest_elapsed_bucket, 3);
    assert_eq!(log.entries.lock().unwrap().len(), 2);
    Ok(())
}

#[test]
fn timing_recovery_merges_raw_receipts_even_after_older_compaction_copies() {
    let limits = limits();
    let initial = TimingReceipt::initial(origin(), limits.invocation_id.as_str().into());
    let advanced = TimingReceipt::new(
        origin(),
        initial.invocation_id.clone(),
        MilestoneCoverage::INITIAL | MilestoneCoverage::ALL_PERCENTAGES,
        3,
    );
    let entries = vec![
        (1, entry(&initial)),
        (2, entry(&advanced)),
        (
            3,
            SessionLogEntry::Compress {
                prompt: "summary".into(),
            },
        ),
        (4, entry(&initial)),
    ];
    assert_eq!(recover_progress(&entries, Some(&limits)), Some(advanced));
}

#[test]
fn timing_recovery_ignores_foreign_invocation_origin_role_and_schema() {
    let limits = limits();
    let valid = TimingReceipt::initial(origin(), limits.invocation_id.as_str().into());
    let mut foreign = valid.clone();
    foreign.invocation_id = "parent".into();
    let mut wrong_origin = valid.clone();
    wrong_origin.origin -= Duration::seconds(1);
    let mut wrong_version = valid.clone();
    wrong_version.version = "v2".into();
    let mut assistant = entry(&valid);
    if let SessionLogEntry::Message { role, .. } = &mut assistant {
        *role = MessageRole::Assistant;
    }
    let entries = vec![
        (1, entry(&foreign)),
        (2, entry(&wrong_origin)),
        (3, entry(&wrong_version)),
        (4, assistant),
    ];
    assert!(recover_progress(&entries, Some(&limits)).is_none());
    let mut entries = entries;
    entries.push((5, entry(&valid)));
    assert_eq!(recover_progress(&entries, Some(&limits)), Some(valid));
}

#[test]
fn timing_recovery_new_invocation_same_conversation_gets_fresh_initial() -> Result<()> {
    let log = Arc::new(Log::default());
    let config = config(&log, Some(limits()));
    start_run(&config, origin());
    prepare(&config, origin() + Duration::seconds(2800))?;
    let mut next_limits = limits();
    next_limits.invocation_id = InvocationIdentity::from_string("next".into());
    next_limits.admitted_at += Duration::seconds(3000);
    next_limits.deadline = Some(next_limits.admitted_at + Duration::seconds(3600));
    let progress = recover_progress(&log.entries.lock().unwrap(), Some(&next_limits));
    assert!(progress.is_none());
    config.write().run_context = Some(next_limits.clone());
    hydrate_run(&config, progress);
    start_run(&config, next_limits.admitted_at);
    let notice = prepare(&config, next_limits.admitted_at)?
        .timing_context
        .unwrap()
        .receipt;
    assert_eq!(notice.invocation_id, "next");
    assert_eq!(notice.origin, next_limits.admitted_at);
    assert_eq!(notice.coverage, MilestoneCoverage::INITIAL);
    assert_eq!(notice.highest_elapsed_bucket, 0);
    Ok(())
}

#[test]
fn timing_recovery_unmanaged_replay_uses_origin_once_then_new_entry_resets() -> Result<()> {
    let log = Arc::new(Log::default());
    let receipt = TimingReceipt::initial(origin(), "unmanaged-replay".into());
    log.append(&entry(&receipt))?;
    let config = config(&log, None);
    hydrate_run(
        &config,
        recover_progress(&log.entries.lock().unwrap(), None),
    );
    let now = origin() + Duration::seconds(900);
    start_run(&config, now);
    let next = prepare(&config, now)?.timing_context.unwrap().receipt;
    assert_eq!(next.origin, origin());
    assert_eq!(next.invocation_id, receipt.invocation_id);
    assert_eq!(next.highest_elapsed_bucket, 1);
    start_run(&config, now);
    let fresh = prepare(&config, now)?.timing_context.unwrap().receipt;
    assert_ne!(fresh.invocation_id, next.invocation_id);
    assert_eq!(fresh.origin, now);
    assert_eq!(fresh.highest_elapsed_bucket, 0);
    Ok(())
}

#[test]
fn timing_recovery_unmanaged_completed_turn_does_not_reuse_old_clock() {
    let receipt = TimingReceipt::initial(origin(), "old".into());
    let terminal = SessionLogEntry::Message {
        id: None,
        role: MessageRole::Assistant,
        content: MessageContent::Text("done".into()),
        timestamp: None,
        fence_token: None,
    };
    assert!(recover_progress(&[(1, entry(&receipt)), (2, terminal)], None).is_none());
}

#[test]
fn timing_recovery_fork_and_storage_scope_do_not_share_progress() -> Result<()> {
    let log = Arc::new(Log::default());
    let config = config(&log, Some(limits()));
    hydrate_run(
        &config,
        Some(TimingReceipt::initial(origin(), "invocation".into())),
    );
    let mut fork = config.read().fork_session_scope();
    assert!(fork.timing_run.is_none());
    fork.session = config.read().session.clone();
    let fork = Arc::new(ConfigLock::new(fork));
    start_run(&fork, origin());
    assert!(prepare(&fork, origin())?.timing_context.is_some());
    let other_log = Arc::new(Log::default());
    config.write().session.as_mut().unwrap().runtime =
        Some(Arc::new(other_log.clone() as Arc<dyn SessionAppendSink>));
    start_run(&config, origin());
    assert!(prepare(&config, origin())?.timing_context.is_some());
    assert_eq!(other_log.entries.lock().unwrap().len(), 1);
    config.write().session.as_mut().unwrap().id = "different-session".into();
    start_run(&config, origin());
    assert!(prepare(&config, origin())?.timing_context.is_some());
    Ok(())
}

#[test]
fn timing_recovery_child_uses_own_admission_and_clamped_deadline() -> Result<()> {
    let parent = limits();
    let child_at = origin() + Duration::seconds(1800);
    let child = RunLimitsRecord::admit_child(
        &parent,
        InvocationIdentity::from_string("child".into()),
        InvocationEdgeKind::Delegation,
        child_at,
        Default::default(),
        None,
        CallTimeoutOverride::from_optional(Some(7200)),
    )?;
    assert_eq!(child.deadline, parent.deadline);
    let log = Arc::new(Log::default());
    let config = config(&log, Some(child.clone()));
    let foreign = vec![(
        1,
        entry(&TimingReceipt::new(
            origin(),
            "invocation".into(),
            MilestoneCoverage::INITIAL | MilestoneCoverage::ALL_PERCENTAGES,
            3,
        )),
    )];
    hydrate_run(&config, recover_progress(&foreign, Some(&child)));
    start_run(&config, child_at);
    let initial = prepare(&config, child_at)?.timing_context.unwrap();
    assert_eq!(initial.receipt.origin, child_at);
    assert_eq!(initial.receipt.coverage, MilestoneCoverage::INITIAL);
    assert!(initial.message.content.to_text().contains(&format!(
        "deadline: {}",
        parent.deadline.unwrap().format("%Y-%m-%dT%H:%M:%SZ")
    )));
    let half = prepare(&config, child_at + Duration::seconds(900))?
        .timing_context
        .unwrap()
        .receipt;
    assert!(half.coverage.contains(MilestoneCoverage::PERCENT_50));
    assert!(!half.coverage.contains(MilestoneCoverage::PERCENT_75));
    assert_eq!(half.highest_elapsed_bucket, 1);
    assert_eq!(
        config.read().timing_run.as_ref().unwrap().limits,
        Some(child.clone())
    );
    let restarted = self::config(&log, Some(child.clone()));
    hydrate_run(
        &restarted,
        recover_progress(&log.entries.lock().unwrap(), Some(&child)),
    );
    start_run(&restarted, child_at + Duration::seconds(900));
    assert!(prepare(&restarted, child_at + Duration::seconds(900))?
        .timing_context
        .is_none());
    let next = prepare(&restarted, child_at + Duration::seconds(1350))?
        .timing_context
        .unwrap()
        .receipt;
    assert_eq!(next.origin, child_at);
    assert_eq!(next.invocation_id, "child");
    assert!(next.coverage.contains(MilestoneCoverage::PERCENT_75));
    assert!(!next.coverage.contains(MilestoneCoverage::PERCENT_90));
    assert!(prepare(&config, parent.deadline.unwrap())?
        .timing_context
        .is_none());
    Ok(())
}

#[test]
fn timing_recovery_unmanaged_terminators_and_new_prompt_reset_clock() {
    let receipt = TimingReceipt::initial(origin(), "invocation".into());
    let new_prompt = SessionLogEntry::Message {
        id: Some("new".into()),
        role: MessageRole::User,
        content: MessageContent::Text("new work".into()),
        timestamp: None,
        fence_token: None,
    };
    for boundary in [
        SessionLogEntry::Cancel {
            fence_token: 0,
            cancellation_id: None,
            requested_by: None,
            timestamp: None,
        },
        SessionLogEntry::TurnEnd {
            through_seq: 1,
            fence_token: 0,
            timestamp: None,
            usage: None,
        },
        SessionLogEntry::Error {
            message: "failed".into(),
            fence_token: 0,
            timestamp: None,
        },
        SessionLogEntry::Clear,
        SessionLogEntry::Rewind { after_seq: 0 },
        new_prompt,
    ] {
        let entries = vec![(1, entry(&receipt)), (2, boundary)];
        assert!(
            recover_progress(&entries, None).is_none(),
            "unmanaged boundary: {entries:?}"
        );
        assert_eq!(
            recover_progress(&entries, Some(&limits())),
            Some(receipt.clone()),
            "managed context mutations cannot uncommit durable emission"
        );
    }
}

#[test]
fn timing_recovery_unmanaged_compaction_copies_do_not_resurrect_old_run() {
    let old = TimingReceipt::initial(origin(), "old".into());
    let current = TimingReceipt::initial(origin() + Duration::seconds(10), "current".into());
    let prompt = SessionLogEntry::Message {
        id: Some("current-prompt".into()),
        role: MessageRole::User,
        content: MessageContent::Text("current work".into()),
        timestamp: None,
        fence_token: None,
    };
    let entries = vec![
        (1, entry(&old)),
        (2, prompt.clone()),
        (3, entry(&current)),
        (
            4,
            SessionLogEntry::Compress {
                prompt: "summary".into(),
            },
        ),
        (5, entry(&old)),
        (6, prompt),
    ];
    assert_eq!(recover_progress(&entries, None), Some(current));
}
