#[path = "hitl_progress_tests.rs"]
mod hitl_progress;
#[path = "interaction_continuation_tests.rs"]
mod interaction_continuation;

use super::{
    derive_hitl_tool_round_continuation, derive_pending_compaction, derive_pending_hitl_approvals,
    dispatch_session_start, find_orphan_tool_calls, SessionOrigin, SessionStartDispatch,
};
use crate::config::Config;
use crate::nats_worker::tests::env_lock;
use chrono::{TimeZone, Utc};
use harnx_core::message::{MessageContent, MessageRole};
use harnx_core::session::SessionLogEntry;
use std::path::PathBuf;
use std::sync::Arc;

fn compact_request(id: &str) -> SessionLogEntry {
    SessionLogEntry::compact_request(id, Some("test".into()))
}

fn compact_result(id: &str) -> SessionLogEntry {
    SessionLogEntry::compact_result(id, harnx_core::session::CompactOutcome::Compacted)
}
#[test]
fn derive_pending_compaction_covers_request_receipt_sequences() {
    let cases = [
        ("empty", vec![], None),
        (
            "unresolved",
            vec![compact_request("comp-1")],
            Some("comp-1"),
        ),
        (
            "matching receipt",
            vec![compact_request("comp-1"), compact_result("comp-1")],
            None,
        ),
        (
            "different receipt",
            vec![compact_request("comp-1"), compact_result("comp-2")],
            Some("comp-1"),
        ),
        (
            "most recent request",
            vec![compact_request("comp-1"), compact_request("comp-2")],
            Some("comp-2"),
        ),
        (
            "older request resolved",
            vec![
                compact_request("comp-1"),
                compact_request("comp-2"),
                compact_result("comp-1"),
            ],
            Some("comp-2"),
        ),
        (
            "interleaved receipts",
            vec![
                compact_request("comp-1"),
                compact_result("comp-1"),
                compact_request("comp-2"),
                compact_request("comp-3"),
            ],
            Some("comp-3"),
        ),
    ];
    for (name, entries, expected) in cases {
        let entries = entries
            .into_iter()
            .enumerate()
            .map(|(i, entry)| (i as u64 + 1, entry))
            .collect::<Vec<_>>();
        assert_eq!(
            derive_pending_compaction(&entries).as_deref(),
            expected,
            "{name}"
        );
    }
}

fn session_start_event_config(
    bash: &str,
    on_error: harnx_core::hooks::EventHookOnError,
) -> std::sync::Arc<crate::config::ConfigLock> {
    let hooks = harnx_core::hooks::HooksConfig {
        session_start: harnx_core::hooks::EventHooksList {
            entries: vec![harnx_core::hooks::EventHookEntry::Full(
                harnx_core::hooks::EventHookDefinition {
                    name: "greet".to_string(),
                    source: harnx_core::middleware::MiddlewareSourceConfig::Bash {
                        bash: bash.to_string(),
                    },
                    fetch: vec![],
                    timeout: std::time::Duration::from_secs(30),
                    on_error,
                },
            )],
        },
        ..Default::default()
    };
    let config = Config {
        data: harnx_core::config_data::ConfigData {
            hooks: Some(hooks),
            ..Default::default()
        },
        ..Config::default()
    };
    std::sync::Arc::new(crate::config::ConfigLock::new(config))
}

async fn dispatch_test_session_start(
    origin: SessionOrigin,
    bash: &str,
    on_error: harnx_core::hooks::EventHookOnError,
) -> (anyhow::Result<()>, Arc<tokio::sync::Mutex<Option<String>>>) {
    let config = session_start_event_config(bash, on_error);
    let pending = Arc::new(tokio::sync::Mutex::new(None));
    let result = dispatch_session_start(
        SessionStartDispatch {
            origin,
            session_id: "fresh-session",
            cwd: PathBuf::from("/tmp/project"),
            model: "test:test-model".to_string(),
            pending_async_context: Some(pending.clone()),
        },
        &config,
        &crate::utils::create_abort_signal(),
    )
    .await;
    (result, pending)
}

#[tokio::test]
async fn created_session_dispatches_session_start_to_worker_hooks() {
    let _guard = env_lock().await;
    let (result, pending) = dispatch_test_session_start(
        SessionOrigin::Created,
        "add_context \"context from session start\"",
        harnx_core::hooks::EventHookOnError::Allow,
    )
    .await;
    result.expect("session start hooks run");
    assert_eq!(
        pending.lock().await.as_deref(),
        Some("context from session start")
    );
}

#[tokio::test]
async fn resumed_session_does_not_redispatch_session_start() {
    let _guard = env_lock().await;
    let (result, pending) = dispatch_test_session_start(
        SessionOrigin::Resumed,
        "add_context \"must not run\"",
        harnx_core::hooks::EventHookOnError::Allow,
    )
    .await;
    result.expect("dispatch succeeds");
    assert!(pending.lock().await.is_none());
}

/// A `deny` SessionStart hook failure must fail the activation instead of
/// being swallowed: setup_agent_loop_context propagates this error.
#[tokio::test]
async fn session_start_hook_deny_failure_propagates() {
    let _guard = env_lock().await;
    let (result, _pending) = dispatch_test_session_start(
        SessionOrigin::Created,
        "exit 3",
        harnx_core::hooks::EventHookOnError::Deny,
    )
    .await;
    let error = result.expect_err("deny must fail session start");
    assert!(error.to_string().contains("greet"), "{error:#}");
}

pub(super) fn user_entry(
    id: &str,
    text: &str,
    timestamp: chrono::DateTime<Utc>,
) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: Some(id.to_string()),
        role: MessageRole::User,
        content: MessageContent::Text(text.to_string()),
        timestamp: Some(timestamp),
        fence_token: None,
    }
}

fn tool_calls_entry(call_id: &str) -> SessionLogEntry {
    SessionLogEntry::ToolCalls {
        text: "working".to_string(),
        thought: None,
        calls: vec![harnx_core::tool::ToolCall::new(
            "search".to_string(),
            serde_json::json!({}),
            Some(call_id.to_string()),
            None,
        )],
        timestamp: None,
        fence_token: Some(7),
    }
}

fn tool_results_entry(call_id: &str) -> SessionLogEntry {
    SessionLogEntry::ToolResults {
        handoff_selection: None,
        results: vec![harnx_core::session::ToolOutput {
            user_surfaces: Vec::new(),
            id: Some(call_id.to_string()),
            name: "search".to_string(),
            output: serde_json::json!({"ok": true}),
            markdown: None,
            content: Vec::new(),
            switch_agent: None,
        }],
        timestamp: None,
    }
}

/// An approved round is dispatched under the sequence of its own
/// `ToolCalls` entry, which entries written while it waited don't move.
#[test]
fn hitl_continuation_resumes_the_round_its_tool_calls_entry_made() {
    let entries = vec![
        (1, tool_calls_entry("earlier-call")),
        (2, tool_results_entry("earlier-call")),
        (3, tool_calls_entry("pending-call")),
        (
            4,
            SessionLogEntry::HitlApprovalRequested {
                tool_call_id: "pending-call".to_string(),
                summary: "Approve pending call".to_string(),
                fence_token: 7,
            },
        ),
        (
            5,
            SessionLogEntry::HitlApprovalDecision {
                tool_call_id: "pending-call".to_string(),
                approved: true,
                note: None,
                fence_token: 7,
            },
        ),
    ];

    let continuation = derive_hitl_tool_round_continuation(&entries)
        .expect("derive HITL continuation")
        .expect("pending round is HITL-managed");
    assert_eq!(continuation.round.seq, 3);
}

#[test]
fn reused_tool_call_id_requires_fresh_approval_in_current_tool_round() {
    let entries = vec![
        (1, tool_calls_entry("reused-call")),
        (
            2,
            SessionLogEntry::HitlApprovalRequested {
                tool_call_id: "reused-call".to_string(),
                summary: "Approve historical call".to_string(),
                fence_token: 7,
            },
        ),
        (
            3,
            SessionLogEntry::HitlApprovalDecision {
                tool_call_id: "reused-call".to_string(),
                approved: true,
                note: None,
                fence_token: 7,
            },
        ),
        (4, tool_results_entry("reused-call")),
        (5, tool_calls_entry("reused-call")),
        (
            6,
            SessionLogEntry::HitlApprovalRequested {
                tool_call_id: "reused-call".to_string(),
                summary: "Approve current call".to_string(),
                fence_token: 8,
            },
        ),
    ];

    let pending = derive_pending_hitl_approvals(&entries).expect("derive pending approval");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].request_seq, 6);
    assert_eq!(pending[0].summary, "Approve current call");

    let continuation = derive_hitl_tool_round_continuation(&entries)
        .expect("derive HITL continuation")
        .expect("current orphan is HITL-managed");
    assert!(
        continuation.decisions.is_empty(),
        "historical approval must not authorize current tool round"
    );
}

#[test]
fn orphan_repair_result_after_queued_user_is_idempotent() {
    let entries = vec![
        (1, tool_calls_entry("call-1")),
        (
            2,
            user_entry(
                "queued",
                "queued correction",
                Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 2).unwrap(),
            ),
        ),
        (3, tool_results_entry("call-1")),
    ];

    assert!(find_orphan_tool_calls(&entries).is_empty());
}

#[test]
fn tool_call_without_results_remains_orphaned_after_queued_user() {
    let entries = vec![
        (1, tool_calls_entry("call-1")),
        (
            2,
            user_entry(
                "queued",
                "queued correction",
                Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 2).unwrap(),
            ),
        ),
    ];

    let orphans = find_orphan_tool_calls(&entries);
    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0].seq, 1);
}

#[test]
fn clear_abandons_all_orphan_batches_and_preserves_new_rounds() {
    let mut entries = vec![
        (1, tool_calls_entry("older-orphan")),
        (2, tool_calls_entry("latest-orphan")),
        (3, SessionLogEntry::Clear),
    ];
    assert!(find_orphan_tool_calls(&entries).is_empty());
    entries.push((4, tool_calls_entry("fresh-call")));
    let orphans = find_orphan_tool_calls(&entries);
    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0].seq, 4);
    assert_eq!(orphans[0].calls[0].id.as_deref(), Some("fresh-call"));
}

#[test]
fn clear_discards_old_consent_even_when_tool_call_id_is_reused() {
    let mut entries = vec![
        (1, tool_calls_entry("reused-call")),
        (
            2,
            SessionLogEntry::HitlApprovalRequested {
                tool_call_id: "reused-call".into(),
                summary: "Old consent".into(),
                fence_token: 7,
            },
        ),
        (
            3,
            SessionLogEntry::HitlApprovalDecision {
                tool_call_id: "reused-call".into(),
                approved: true,
                note: None,
                fence_token: 7,
            },
        ),
        (4, SessionLogEntry::Clear),
    ];
    assert!(derive_pending_hitl_approvals(&entries).unwrap().is_empty());
    assert!(derive_hitl_tool_round_continuation(&entries)
        .unwrap()
        .is_none());
    entries.extend([
        (5, tool_calls_entry("reused-call")),
        (
            6,
            SessionLogEntry::HitlApprovalRequested {
                tool_call_id: "reused-call".into(),
                summary: "Fresh consent".into(),
                fence_token: 8,
            },
        ),
    ]);
    let pending = derive_pending_hitl_approvals(&entries).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].request_seq, 6);
    let continuation = derive_hitl_tool_round_continuation(&entries)
        .unwrap()
        .unwrap();
    assert_eq!(continuation.round.seq, 5);
    assert!(continuation.decisions.is_empty());
}

/// Issue #2493: runtime notes — replay-synthesized context re-logged by
/// compaction — are not executable prompts, whether they carry a structured
/// runtime-note id or only the note prefix on a legacy ID-less row. Admitted
/// prompts that merely start with the prefix keep their id and stay executable.
#[test]
fn executable_user_message_id_skips_runtime_notes_only() {
    use super::executable_user_message_id;
    use harnx_core::message::MessageContent;
    use harnx_core::runtime_note::RUNTIME_NOTE_PREFIX;

    fn user_row(id: Option<&str>, text: &str) -> SessionLogEntry {
        SessionLogEntry::Message {
            id: id.map(str::to_owned),
            role: MessageRole::User,
            content: MessageContent::Text(text.to_string()),
            timestamp: None,
            fence_token: None,
        }
    }

    let note_id = harnx_core::runtime_note::new_runtime_note_message_id();
    // Structured runtime-note row: not executable.
    assert_eq!(
        executable_user_message_id(&user_row(
            Some(&note_id),
            &format!("{RUNTIME_NOTE_PREFIX}Started sub-agent 'child'.")
        )),
        None
    );
    // Hook-added context rows (issue #2198, review F2) are runtime-authored
    // context, not prompts: the mid-turn fold must never select them.
    assert_eq!(
        executable_user_message_id(&user_row(
            Some(&harnx_core::runtime_note::new_hook_context_message_id()),
            "context added by an event hook"
        )),
        None
    );
    // An ID-less lookalike is NOT classified by text: it stays
    // executable-but-unmapped, the signal that fails admission verification.
    assert_eq!(
        executable_user_message_id(&user_row(
            None,
            &format!("{RUNTIME_NOTE_PREFIX}The user interrupted this turn.")
        )),
        Some(None)
    );
    // An admitted lookalike keeps its identity and stays executable.
    assert_eq!(
        executable_user_message_id(&user_row(
            Some("admitted-lookalike"),
            &format!("{RUNTIME_NOTE_PREFIX}lookalike prompt")
        )),
        Some(Some("admitted-lookalike"))
    );
    // An ID-less non-internal row is executable-but-unmapped: Some(None), the
    // signal that fails admission verification.
    assert_eq!(
        executable_user_message_id(&user_row(None, "unadmitted input")),
        Some(None)
    );
}
