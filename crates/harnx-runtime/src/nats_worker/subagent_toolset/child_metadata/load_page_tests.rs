//! Refit-ladder unit tests (plan #2970, T3): stalls in both cursor modes,
//! optional metadata and optional progress reduced or dropped against the
//! SAME window, mandatory-only overflow, and cursor-neutrality. No broker:
//! every case fits an in-memory window.

use super::*;
use crate::nats_worker::subagent_toolset::session_read::ReadOptions;
use harnx_core::message::{MessageContent, MessageRole};
use harnx_core::pending_interaction::{ApprovalRef, PendingInteractionRequest};
use harnx_core::session::SessionLogEntry;
use serde_json::json;

const SESSION: &str = "child-1";

fn user(text: &str) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: None,
        role: MessageRole::User,
        content: MessageContent::Text(text.into()),
        timestamp: None,
        fence_token: None,
    }
}

fn window(texts: &[&str]) -> LoadedWindow {
    LoadedWindow::Sequential {
        entries: texts
            .iter()
            .enumerate()
            .map(|(i, text)| ((i + 1) as u64, user(text)))
            .collect(),
        capped: false,
        scanned_through: None,
    }
}

fn continuation_window(text: &str) -> LoadedWindow {
    let entry = user(text);
    LoadedWindow::Continuation {
        entries: vec![(1, entry)],
    }
}

fn resolve_read(options: serde_json::Value) -> ResolvedRead {
    serde_json::from_value::<ReadOptions>(options)
        .expect("read options")
        .resolve()
        .expect("read resolves")
}

fn scope(session_id: &str, prompt: Option<(&str, bool)>, pending: Option<BigPending>) -> LoadScope {
    LoadScope {
        session_id: session_id.to_string(),
        prompt_id: prompt.map(|(id, _)| id.to_string()),
        status: prompt.map(|(_, blocked)| {
            if blocked {
                SessionPromptStatus::Blocked
            } else {
                SessionPromptStatus::Pending
            }
        }),
        pending: pending.map(|pending| pending.summary()),
    }
}

struct BigPending {
    message_bytes: usize,
}

impl BigPending {
    fn summary(&self) -> PendingInteractionSummary {
        PendingInteractionSummary {
            agent: "helper".into(),
            cluster: "__local__".into(),
            session_id: SESSION.into(),
            request: PendingInteractionRequest::LegacyApproval {
                request: ApprovalRef {
                    invocation_id: "prompt-1".into(),
                    tool_round_seq: 1,
                    request_seq: 2,
                    tool_call_id: "call-1".into(),
                },
            },
            tool_name: "bash_exec".into(),
            message: "m".repeat(self.message_bytes),
            pending_count: 0,
        }
    }
}

/// A metadata projection that always claims its full allowance.
fn big_metadata() -> LoadMetadata {
    let view = json!({
        "session_id": SESSION,
        "agent": "helper",
        "title": "t".repeat(160),
        "truncated": false,
    });
    LoadMetadata::Bounded(view)
}

/// Shared by both progress-stall tests: a blocked prompt whose pending
/// identity alone survives the minimum budget, refit with no metadata.
fn refit_pending_identity(window: &LoadedWindow, read: &ResolvedRead) -> serde_json::Value {
    let scope = scope(
        SESSION,
        Some(("prompt-1", true)),
        Some(BigPending { message_bytes: 512 }),
    );
    let result = fit(window, read, &scope, &LoadMetadata::Absent).expect("refit");
    assert_fits_and_advances(&result, 1024, read, Some(json!(null)));
    assert!(
        result["pending_interaction"].is_object(),
        "the mandatory pending-interaction identity survives every refit"
    );
    result
}

fn fit(
    window: &LoadedWindow,
    read: &ResolvedRead,
    scope: &LoadScope,
    metadata: &LoadMetadata,
) -> Result<serde_json::Value, ToolInvokeError> {
    fit_loaded_window(window, read, scope, metadata)
}

fn serialized(value: &serde_json::Value) -> usize {
    serde_json::to_vec(value).expect("serializes").len()
}

fn assert_fits_and_advances(
    result: &serde_json::Value,
    budget: usize,
    read: &ResolvedRead,
    expected_metadata: Option<serde_json::Value>,
) {
    let size = serialized(result);
    assert!(size <= budget, "final serialized size {size} fits {budget}");
    match read.mode {
        crate::nats_worker::subagent_toolset::session_read::ReadMode::Sequential(_) => {
            assert!(
                !result["events"].as_array().expect("events").is_empty(),
                "an indexed page delivers content"
            );
            assert!(result["next_index"].is_null() || result["truncated"] == json!(true));
        }
        crate::nats_worker::subagent_toolset::session_read::ReadMode::Continuation { .. } => {
            assert!(
                !result["text"].as_str().expect("text").is_empty(),
                "a byte page delivers content"
            );
            assert!(result["continuation"]["byte_offset"].as_u64().unwrap() > 0);
        }
    }
    match expected_metadata {
        Some(metadata) => assert_eq!(result["metadata"], metadata),
        None => assert!(
            result.get("metadata").is_none(),
            "unfit metadata is omitted"
        ),
    }
}

#[test]
fn metadata_stall_in_indexed_mode_refits_the_same_window() {
    let session = "s".repeat(600);
    let window = window(&["alpha", "beta", "gamma"]);
    let read = resolve_read(json!({"max_output_bytes": 1024}));
    let scope = scope(&session, None, None);
    // Detection: for some metadata-heavy reserve the page delivers no events
    // although matches exist, and points back at its first undelivered match.
    let stall_reserve = (724..924)
        .find(|reserve| {
            matches!(
                session_read::fit_read_window(&window, &read, *reserve),
                Ok(outcome) if outcome.stalled
            )
        })
        .expect("an indexed page with matched events must report the stall");
    assert!(
        stall_reserve > 724,
        "the stall case is a real window: {stall_reserve}"
    );

    let result = fit(&window, &read, &scope, &big_metadata()).expect("refit");
    // The same original window, refit: mandatory identity is preserved, the
    // page advances, and the metadata is reduced to its marker.
    assert_fits_and_advances(&result, 1024, &read, Some(json!({"truncated": true})));
}

#[test]
fn progress_stall_in_indexed_mode_refits_to_pending_identity() {
    let window = window(&["alpha", "beta"]);
    let read = resolve_read(json!({"max_output_bytes": 1024}));
    let result = refit_pending_identity(&window, &read);
    assert_eq!(result["status"], json!("blocked"));
    assert!(
        result.get("sub_agent_progress").is_none(),
        "optional progress is dropped before the mandatory page"
    );
    let message = result["pending_interaction"]["message"]
        .as_str()
        .expect("message");
    assert!(message.len() <= 512);
}

#[test]
fn metadata_stall_in_byte_mode_refits_with_an_advancing_cursor() {
    let session = "s".repeat(700);
    let window = continuation_window(&"z".repeat(4000));
    let read = resolve_read(json!({"entry_index": 1, "byte_offset": 0, "max_output_bytes": 1024}));
    let scope = scope(&session, None, None);
    // Detection: a continuation that must advance its byte offset but
    // delivers nothing reports the stall instead of looping.
    assert!(
        (724..924).any(|reserve| {
            matches!(
                session_read::fit_read_window(&window, &read, reserve),
                Ok(outcome) if outcome.stalled
            )
        }),
        "a byte continuation must report the stall it would loop on"
    );

    let result = fit(&window, &read, &scope, &big_metadata()).expect("refit");
    assert_fits_and_advances(&result, 1024, &read, Some(json!({"truncated": true})));
    let next = result["continuation"]["byte_offset"]
        .as_u64()
        .expect("advancing cursor");
    assert!(
        next > 0,
        "the refit advances the byte offset instead of repeating it"
    );
}

#[test]
fn byte_mode_omits_metadata_to_preserve_mandatory_identities_and_progress() {
    let window = continuation_window(&"z".repeat(4000));
    let read = resolve_read(json!({
        "entry_index": 1,
        "byte_offset": 0,
        "max_output_bytes": 1024
    }));
    // This identifier length is the boundary where the marker does not fit
    // with an advancing byte page, but omission does.
    let session = "s".repeat(488);
    let scope = scope(
        &session,
        Some(("prompt-routing-identity", true)),
        Some(BigPending { message_bytes: 512 }),
    );
    assert!(
        Attempt::MarkerMetadata
            .fit(&window, &read, &scope, &big_metadata())
            .is_none(),
        "marker metadata cannot preserve byte progress at this boundary"
    );
    assert!(
        Attempt::NoMetadata
            .fit(&window, &read, &scope, &big_metadata())
            .is_some(),
        "omitting metadata allows byte progress at the same boundary"
    );
    let result = fit(&window, &read, &scope, &big_metadata()).expect("omission recovers progress");

    assert_fits_and_advances(&result, 1024, &read, None);
    assert_eq!(result["session_id"], json!(session));
    assert_eq!(result["prompt_id"], json!("prompt-routing-identity"));
    assert_eq!(result["status"], json!("blocked"));
    assert_eq!(result["terminal"], json!(false));

    let pending = &result["pending_interaction"];
    assert_eq!(pending["agent"], json!("helper"));
    assert_eq!(pending["cluster"], json!("__local__"));
    assert_eq!(pending["session_id"], json!(SESSION));
    assert_eq!(pending["kind"], json!("legacy_approval"));
    assert_eq!(pending["request"]["invocation_id"], json!("prompt-1"));
    assert_eq!(pending["request"]["tool_round_seq"], json!(1));
    assert_eq!(pending["request"]["request_seq"], json!(2));
    assert_eq!(pending["request"]["tool_call_id"], json!("call-1"));
    assert_eq!(pending["tool_name"], json!("bash_exec"));
}

#[test]
fn progress_stall_in_byte_mode_refits_with_an_advancing_cursor() {
    let window = continuation_window(&"z".repeat(4000));
    let read = resolve_read(json!({"entry_index": 1, "byte_offset": 0, "max_output_bytes": 1024}));
    let result = refit_pending_identity(&window, &read);
    assert!(result.get("sub_agent_progress").is_none());
}

#[test]
fn end_of_data_is_never_a_stall() {
    let window = window(&["only"]);
    let read = resolve_read(json!({"index_min": 2, "max_output_bytes": 1024}));
    let scope = scope(SESSION, None, None);
    let result = fit(&window, &read, &scope, &LoadMetadata::Absent).expect("end of data");
    assert_eq!(result["events"], json!([]));
    assert_eq!(result["metadata"], json!(null));

    let exhausted =
        resolve_read(json!({"entry_index": 1, "byte_offset": 0, "max_output_bytes": 1024}));
    let window = continuation_window("tiny");
    let result = fit(&window, &exhausted, &scope, &LoadMetadata::Absent).expect("end of data");
    assert_eq!(result["truncated"], json!(false));
    assert_eq!(result["continuation"], json!(null));
}

#[test]
fn mandatory_only_overflow_returns_the_increase_budget_error() {
    let session = "s".repeat(2000);
    let window = window(&["alpha"]);
    let read = resolve_read(json!({"max_output_bytes": 1024}));
    let scope = scope(&session, None, None);
    let error = fit(&window, &read, &scope, &LoadMetadata::Absent)
        .expect_err("mandatory identity alone exceeds the minimum budget");
    let message = error.to_string();
    assert!(
        message.contains("read.max_output_bytes") && message.contains("larger"),
        "bounded recoverable ask for a larger budget: {message}"
    );
    assert!(
        message.len() < 300,
        "the error itself stays bounded: {message}"
    );
}

#[test]
fn refits_never_consume_a_cursor_and_stay_deterministic() {
    let window = window(&["alpha", "beta"]);
    let read = resolve_read(json!({"max_output_bytes": 1024}));
    let scope = scope(SESSION, None, None);
    let first = fit(&window, &read, &scope, &LoadMetadata::Absent).expect("fits");
    let second = fit(&window, &read, &scope, &LoadMetadata::Absent).expect("fits");
    assert_eq!(
        first, second,
        "refitting the same window yields the same page"
    );
}

#[test]
fn default_and_maximum_budgets_keep_metadata_visible() {
    let window = window(&["alpha"]);
    let metadata = big_metadata();
    for budget in [1024usize, 131_072, 307_200] {
        let read = resolve_read(json!({"max_output_bytes": budget}));
        let scope = scope(SESSION, None, None);
        let result = fit(&window, &read, &scope, &metadata).expect("fits");
        assert!(serialized(&result) <= budget);
        // At the minimum budget the allowance claims a quarter of the page,
        // so the full metadata may be reduced; at the default and maximum it
        // is visible whole.
        if budget > 1024 {
            assert!(
                result["metadata"].is_object(),
                "ordinary budgets keep metadata"
            );
        }
    }
}
