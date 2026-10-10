use super::{
    agent_resolved_hooks, derive_hitl_tool_round_continuation, derive_pending_compaction,
    derive_pending_hitl_approvals, dispatch_session_start, find_orphan_tool_calls, SessionOrigin,
    SessionStartDispatch,
};
use crate::config::Config;
use crate::nats_hook_provider::{DiscoveredHook, NatsHookProvider};
use chrono::{TimeZone, Utc};
use harnx_core::hooks::{HookEvent, HookOutcome, HookPayload, HookResult, HookResultControl};
use harnx_core::instance::ServerScope;
use harnx_core::message::{MessageContent, MessageRole};
use harnx_core::session::SessionLogEntry;
use harnx_hookset::{FailPolicy, HookSpec};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

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

/// A provider whose only route is a SessionStart hook recording every
/// payload it receives.
fn recording_session_start_provider() -> (NatsHookProvider, Arc<Mutex<Vec<HookPayload>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen);
    let provider = NatsHookProvider::from_request_handler(
        ServerScope::from_string("session-start-test"),
        vec![DiscoveredHook {
            server: "lifecycle".to_string(),
            display_label: None,
            spec: HookSpec {
                event: "SessionStart".to_string(),
                matcher: None,
                priority: 0,
                timeout_secs: Some(1),
                fail_policy: FailPolicy::Closed,
            },
        }],
        Arc::new(move |_subject, payload: HookPayload| {
            recorder.lock().expect("recorder lock").push(payload);
            HookOutcome {
                control: HookResultControl::Continue,
                result: HookResult::default(),
            }
        }),
    );
    (provider, seen)
}

#[tokio::test]
async fn created_session_dispatches_session_start_to_worker_hooks() {
    let (provider, seen) = recording_session_start_provider();

    dispatch_session_start(SessionStartDispatch {
        abort: None,
        origin: SessionOrigin::Created,
        provider: Some(&provider),
        session_id: "fresh-session",
        cwd: PathBuf::from("/tmp/project"),
        model: "test:test-model".to_string(),
        pending_async_context: None,
    })
    .await;

    let seen = seen.lock().expect("recorder lock");
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].session_id, "fresh-session");
    assert_eq!(seen[0].cwd, PathBuf::from("/tmp/project"));
    let HookEvent::SessionStart { source, model } = &seen[0].hook_event else {
        panic!("expected SessionStart, got {:?}", seen[0].hook_event);
    };
    assert_eq!(source, "startup");
    assert_eq!(model, "test:test-model");
}

#[tokio::test]
async fn resumed_session_does_not_redispatch_session_start() {
    let (provider, seen) = recording_session_start_provider();

    dispatch_session_start(SessionStartDispatch {
        abort: None,
        origin: SessionOrigin::Resumed,
        provider: Some(&provider),
        session_id: "existing-session",
        cwd: PathBuf::from("/tmp/project"),
        model: "test:test-model".to_string(),
        pending_async_context: None,
    })
    .await;

    assert!(seen.lock().expect("recorder lock").is_empty());
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
        results: vec![harnx_core::session::ToolOutput {
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
    assert_eq!(pending[0].seq, 6);
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
fn session_hook_resolution_excludes_instance_hooks() {
    let config = Config {
        data: harnx_core::config_data::ConfigData {
            hooks: Some(harnx_core::hooks::HooksConfig {
                max_resume: None,
                entries: vec![harnx_core::hooks::HookConfig {
                    command: "harnx-claude-compatible-hook-server --event SessionStart --timeout 30 -- echo global".to_string(),
                    status_message: None,
                    async_hook: None,
                    package_dir: None,
                }],
            }),
            ..harnx_core::config_data::ConfigData::default()
        },
        ..Config::default()
    };
    let config = std::sync::Arc::new(crate::config::ConfigLock::new(config));

    assert!(agent_resolved_hooks(&config).entries.is_empty());
}

#[test]
fn session_hook_resolution_keeps_agent_override_of_global_hook() {
    let global_hook = harnx_core::hooks::HookConfig {
        command:
            "harnx-claude-compatible-hook-server --event SessionStart --timeout 30 -- echo global"
                .to_string(),
        status_message: None,
        async_hook: None,
        package_dir: None,
    };
    let agent_config = harnx_core::agent_config::AgentConfig::from_markdown(
        "override-agent",
        "---\nhooks:\n  entries:\n    - command: harnx-claude-compatible-hook-server --event SessionStart --timeout 30 -- echo agent\n---\nprompt",
    )
    .expect("agent config");
    let config = Config {
        data: harnx_core::config_data::ConfigData {
            hooks: Some(harnx_core::hooks::HooksConfig {
                max_resume: None,
                entries: vec![global_hook],
            }),
            ..harnx_core::config_data::ConfigData::default()
        },
        agent: Some(crate::config::Agent::new(agent_config)),
        ..Config::default()
    };
    let config = std::sync::Arc::new(crate::config::ConfigLock::new(config));

    let hooks = agent_resolved_hooks(&config);
    assert_eq!(hooks.entries.len(), 1);
    assert_eq!(
        hooks.entries[0].command,
        "harnx-claude-compatible-hook-server --event SessionStart --timeout 30 -- echo agent"
    );
}
