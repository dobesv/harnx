#[path = "hitl_progress_support.rs"]
mod hitl_progress_support;
use hitl_progress_support::*;

use super::{tool_calls_entry, tool_results_entry, user_entry};
use crate::nats_session::{fixed_admission::FixedAdmissionTicket, NatsSession};
use crate::{ApprovalRef, PendingHitlApproval, PromptProgress};
use harnx_core::{session::SessionLogEntry, tool::ToolCall};
use serde_json::json;

fn ticket(predecessor: u64) -> FixedAdmissionTicket {
    FixedAdmissionTicket::from_parts(
        "session".into(),
        "invocation".into(),
        "prompt".into(),
        "closure".into(),
        predecessor,
    )
    .unwrap()
}

fn progress(entries: &[(u64, SessionLogEntry)], predecessor: u64) -> PromptProgress {
    NatsSession::fixed_prompt_progress_from_entries(
        &ticket(predecessor),
        predecessor + 1,
        "session",
        entries,
    )
    .unwrap()
}

fn request(id: &str) -> SessionLogEntry {
    SessionLogEntry::HitlApprovalRequested {
        tool_call_id: id.into(),
        summary: format!("Approve {id}?"),
        fence_token: 7,
    }
}

fn decision(id: &str, approved: bool) -> SessionLogEntry {
    SessionLogEntry::HitlApprovalDecision {
        tool_call_id: id.into(),
        approved,
        note: Some("decision note".into()),
        fence_token: 7,
    }
}

fn pending(progress: PromptProgress) -> PendingHitlApproval {
    let PromptProgress::InputRequired(pending) = progress else {
        panic!("expected InputRequired, got {progress:?}");
    };
    pending
}

#[test]
fn hitl_progress_pending_joins_round_and_public_types_round_trip() {
    let mut round = tool_calls_entry("call");
    if let SessionLogEntry::ToolCalls { calls, .. } = &mut round {
        calls[0].arguments = json!({"query": "durable approval", "limit": 2});
    }
    let entries = vec![(2, round), (3, request("call"))];
    let pending = pending(progress(&entries, 0));
    assert_pending_matches(
        &pending,
        ExpectedPending {
            request_seq: 3,
            tool_round_seq: 2,
            tool_call_id: "call",
            tool_name: "search",
            arguments: json!({"query": "durable approval", "limit": 2}),
            summary: "Approve call?",
        },
    );
    let reference = pending.approval_ref("invocation");
    assert_approval_ref_matches(
        &reference,
        &ApprovalRef {
            invocation_id: "invocation".into(),
            request_seq: 3,
            tool_round_seq: 2,
            tool_call_id: "call".into(),
        },
    );
    assert_pending_roundtrip(&pending);
    assert_reference_roundtrip(&reference);
}

#[test]
fn hitl_progress_decided_then_running_reports_exact_request_and_answer() {
    for approved in [true, false] {
        let entries = vec![
            (2, tool_calls_entry("call")),
            (3, request("call")),
            (4, decision("call", approved)),
            (5, tool_results_entry("call")),
        ];
        let PromptProgress::Running { last_decision } = progress(&entries, 0) else {
            panic!("decision without terminal must remain Running");
        };
        let receipt = last_decision.unwrap();
        let expected_ref = pending(progress(&entries[..2], 0)).approval_ref("invocation");
        assert_decision_receipt_matches(&receipt, &expected_ref, 4, approved);
    }
}

#[test]
fn hitl_progress_next_pending_in_same_round_wins_over_last_decision() {
    let mut round = tool_calls_entry("first");
    if let SessionLogEntry::ToolCalls { calls, .. } = &mut round {
        calls.push(ToolCall::new(
            "next_tool".into(),
            json!({"next": true}),
            Some("second".into()),
            None,
        ));
    }
    let entries = vec![
        (2, round),
        (3, request("first")),
        (4, decision("first", true)),
        (5, request("second")),
    ];
    let pending = pending(progress(&entries, 0));
    assert_next_pending(
        &pending,
        ExpectedNextPending {
            request_seq: 5,
            tool_round_seq: 2,
            tool_name: "next_tool",
            arguments: json!({"next": true}),
        },
    );
}

#[test]
fn hitl_progress_reused_tool_call_id_joins_current_round_not_old_decision() {
    let entries = vec![
        (2, tool_calls_entry("reused")),
        (3, request("reused")),
        (4, decision("reused", true)),
        (5, tool_results_entry("reused")),
        (6, tool_calls_entry("reused")),
        (7, request("reused")),
    ];
    let current = pending(progress(&entries, 0));
    assert_eq!(current.tool_round_seq, 6);
    assert_eq!(current.request_seq, 7);
    assert_ne!(
        current.approval_ref("invocation"),
        pending(progress(&entries[..2], 0)).approval_ref("invocation")
    );
    let mut decided = entries;
    decided.push((8, decision("reused", false)));
    let PromptProgress::Running {
        last_decision: Some(receipt),
    } = progress(&decided, 0)
    else {
        panic!("expected latest decision");
    };
    assert_eq!(receipt.request, current.approval_ref("invocation"));
    assert!(!receipt.approved);
}

#[test]
fn hitl_progress_request_after_prompt_cannot_claim_round_before_prompt() {
    let entries = vec![
        (1, tool_calls_entry("older-invocation")),
        (
            2,
            user_entry("prompt", "new invocation", chrono::Utc::now()),
        ),
        (3, request("older-invocation")),
    ];
    assert!(matches!(
        progress(&entries, 1),
        PromptProgress::Running {
            last_decision: None
        }
    ));
    let mut decided = entries;
    decided.push((4, decision("older-invocation", true)));
    assert!(matches!(
        progress(&decided, 1),
        PromptProgress::Running {
            last_decision: None
        }
    ));
}

#[test]
fn hitl_progress_terminal_wins_over_pending_and_later_invocations() {
    let terminators = [
        SessionLogEntry::TurnEnd {
            through_seq: 1,
            fence_token: 7,
            timestamp: None,
            usage: None,
        },
        SessionLogEntry::Cancel {
            fence_token: 7,
            cancellation_id: None,
            requested_by: None,
            timestamp: None,
        },
        SessionLogEntry::Error {
            message: "worker stopped".into(),
            fence_token: 7,
            timestamp: None,
        },
    ];
    for terminal in terminators {
        let cancelled = matches!(terminal, SessionLogEntry::Cancel { .. });
        let failed = matches!(terminal, SessionLogEntry::Error { .. });
        let entries = vec![
            (2, tool_calls_entry("reused")),
            (3, request("reused")),
            (4, terminal),
            (
                5,
                user_entry("next-prompt", "next invocation", chrono::Utc::now()),
            ),
            (6, tool_calls_entry("reused")),
            (7, request("reused")),
        ];
        let PromptProgress::Terminal(result) = progress(&entries, 0) else {
            panic!("terminal proof must win");
        };
        assert_terminal_turn_result(&result, cancelled, failed.then_some("worker stopped"));
        assert_eq!(pending(progress(&entries, 4)).tool_round_seq, 6);
    }
}

#[test]
fn hitl_progress_without_approval_is_running() {
    assert!(matches!(
        progress(&[], 0),
        PromptProgress::Running {
            last_decision: None
        }
    ));
}

#[test]
fn hitl_progress_ignores_decisions_without_matching_round_request() {
    for entries in [
        vec![
            (2, tool_calls_entry("call")),
            (3, request("wrong-id")),
            (4, decision("wrong-id", true)),
        ],
        vec![(2, tool_calls_entry("call")), (3, decision("call", true))],
        vec![
            (2, tool_calls_entry("call")),
            (3, request("call")),
            (4, tool_results_entry("call")),
            (5, decision("call", true)),
        ],
    ] {
        assert!(matches!(
            progress(&entries, 0),
            PromptProgress::Running {
                last_decision: None
            }
        ));
    }
}

#[test]
fn hitl_progress_rewind_removes_decision_but_cannot_erase_raw_terminal() {
    let mut entries = vec![
        (2, tool_calls_entry("call")),
        (3, request("call")),
        (4, decision("call", true)),
        (5, SessionLogEntry::Rewind { after_seq: 3 }),
    ];
    assert_eq!(pending(progress(&entries, 0)).request_seq, 3);
    entries.push((
        6,
        SessionLogEntry::TurnEnd {
            through_seq: 1,
            fence_token: 7,
            timestamp: None,
            usage: None,
        },
    ));
    entries.push((7, SessionLogEntry::Rewind { after_seq: 3 }));
    assert!(matches!(progress(&entries, 0), PromptProgress::Terminal(_)));
}
