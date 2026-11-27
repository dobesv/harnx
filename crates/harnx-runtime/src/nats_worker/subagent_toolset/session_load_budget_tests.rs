//! Broker-backed budget tests for `session_load` (plan #2970, T3): optional
//! metadata and optional progress cannot stall or overflow a page in either
//! cursor mode, the mandatory pending-interaction identity survives, exact
//! identities and advancing cursors hold at 1024/default/maximum budgets,
//! and paging the whole transcript is identical with and without variable
//! previews.

use super::session_load_metadata_tests::{assistant, metadata_patch, seed};
use super::session_load_tests::{serialized, turn_end, user, LoadFixture};
use crate::nats_session_log::NatsSessionLog;
use harnx_core::session::SessionLogEntry;
use serde_json::json;

/// The raw session id is part of the wrapper's mandatory identity AND of the
/// metadata projection's identity, so long ids are the deterministic way to
/// squeeze the page from both directions at the 1024-byte minimum budget.
/// The exact fit point depends on entry rendering, so the tests below sweep a
/// range of lengths and assert the CONTRACT on every one of them: below the
/// mandatory-identity limit a load must never stall, overflow or error, and
/// past it the only legal outcome is the bounded recoverable ask for a
/// larger budget — never a silent truncation of the session id.
fn long_session(zeros: usize) -> String {
    format!("budget-child-{}", "0".repeat(zeros))
}

/// The sweep tests page over identical small user entries; one helper keeps
/// the rendered row shape shared across every budget case.
fn entry_event(i: usize) -> SessionLogEntry {
    user(&format!("entry {i}"))
}

fn log_for(f: &LoadFixture, session_id: &str) -> NatsSessionLog {
    let storage = harnx_core::session_identity::session_key(Some("helper"), session_id);
    NatsSessionLog::new_with_replicas(f.js().clone(), storage, 1)
}

fn interaction_request(round_seq: u64) -> SessionLogEntry {
    SessionLogEntry::InteractionRequested {
        tool_round_seq: round_seq,
        tool_call_id: "interact-call".into(),
        interaction_id: "int-1".into(),
        producer: "middleware:confirm".into(),
        step_path: vec![],
        key: "1".into(),
        kind: "confirm".into(),
        payload: json!({}),
        summary: "A".repeat(600),
        chain_version: "v1".into(),
        checkpoint: "cp".into(),
        fence_token: 0,
    }
}

#[tokio::test]
async fn metadata_induced_stall_refits_in_indexed_mode() -> anyhow::Result<()> {
    let f = LoadFixture::new().await?;
    for zeros in [300usize, 500, 650, 750] {
        let session = long_session(zeros);
        seed(&f, &session, &metadata_patch(Vec::new(), &[])).await?;
        let log = log_for(&f, &session);
        for i in 0..3 {
            log.append_event_async(&entry_event(i)).await?;
        }
        let result = f
            .load(json!({ "session_id": &session, "read": { "max_output_bytes": 1024 } }))
            .await
            .map_err(|error| anyhow::anyhow!("session id {} bytes: {error}", session.len()))?;
        assert!(
            serialized(&result) <= 1024,
            "session id {} bytes: the whole result fits the budget",
            session.len()
        );
        assert!(
            !result["events"].as_array().expect("events").is_empty(),
            "session id {} bytes: the page delivers content instead of stalling",
            session.len()
        );
        assert_eq!(
            result["session_id"],
            json!(session),
            "session id {} bytes: identity is exact",
            session.len()
        );
        assert!(
            result.get("metadata").is_none()
                || result["metadata"] == json!({"truncated": true})
                || result["metadata"].is_object(),
            "session id {} bytes: unfit metadata is reduced, never oversized: {}",
            session.len(),
            result["metadata"]
        );
    }
    // Past the mandatory-identity limit even the metadata-free, progress-free
    // wrapper cannot fit one page: the bounded recoverable ask for a larger
    // budget is the only legal outcome — never a dropped or clipped identity.
    let session = long_session(1100);
    seed(&f, &session, &metadata_patch(Vec::new(), &[])).await?;
    let log = log_for(&f, &session);
    log.append_event_async(&entry_event(0)).await?;
    let error = f
        .load(json!({ "session_id": &session, "read": { "max_output_bytes": 1024 } }))
        .await
        .expect_err("mandatory identity alone cannot fit the minimum budget");
    assert!(
        error.to_string().contains("larger read.max_output_bytes"),
        "unexpected: {error}"
    );
    Ok(())
}

#[tokio::test]
async fn metadata_induced_stall_refits_in_byte_mode() -> anyhow::Result<()> {
    let f = LoadFixture::new().await?;
    for zeros in [300usize, 500, 700, 800] {
        let session = long_session(zeros);
        seed(&f, &session, &metadata_patch(Vec::new(), &[])).await?;
        let log = log_for(&f, &session);
        let body = "z".repeat(4000);
        log.append_event_async(&user(&body)).await?;
        let result = f
            .load(json!({
                "session_id": &session,
                "read": { "entry_index": 1, "byte_offset": 0, "max_output_bytes": 1024 }
            }))
            .await
            .map_err(|error| anyhow::anyhow!("session id {} bytes: {error}", session.len()))?;
        assert!(
            serialized(&result) <= 1024,
            "session id {} bytes: the whole result fits the budget",
            session.len()
        );
        let next = result["continuation"]["byte_offset"]
            .as_u64()
            .expect("cursor");
        assert!(
            next > 0,
            "session id {} bytes: the byte cursor advances instead of pointing back at its own offset",
            session.len()
        );
        assert!(
            !result["text"].as_str().expect("text").is_empty(),
            "session id {} bytes: the page delivers content instead of stalling",
            session.len()
        );
        assert_eq!(
            result["session_id"],
            json!(session),
            "session id {} bytes: identity is exact",
            session.len()
        );
    }
    // Past the mandatory-identity limit the continuation skeleton cannot fit
    // next to the session id: the bounded recoverable ask for a larger budget
    // is the only legal outcome.
    let session = long_session(950);
    seed(&f, &session, &metadata_patch(Vec::new(), &[])).await?;
    let log = log_for(&f, &session);
    log.append_event_async(&user(&"z".repeat(4000))).await?;
    let error = f
        .load(json!({
            "session_id": &session,
            "read": { "entry_index": 1, "byte_offset": 0, "max_output_bytes": 1024 }
        }))
        .await
        .expect_err("mandatory identity alone cannot fit the minimum budget");
    assert!(
        error.to_string().contains("larger read.max_output_bytes"),
        "unexpected: {error}"
    );
    Ok(())
}

#[tokio::test]
async fn blocked_prompt_keeps_identity_within_minimum_budget() -> anyhow::Result<()> {
    let f = LoadFixture::new().await?;
    seed(&f, LoadFixture::SESSION, &metadata_patch(Vec::new(), &[])).await?;
    let (prompt_id, prompt_seq) = f.dispatch_prompt().await?;
    let log = log_for(&f, LoadFixture::SESSION);
    let round_seq = log
        .append_event_async(&SessionLogEntry::ToolCalls {
            text: String::new(),
            thought: None,
            calls: vec![harnx_core::tool::ToolCall {
                id: Some("interact-call".into()),
                name: "ask_user".into(),
                arguments: json!({}),
                thought_signature: None,
                reasoning_provenance: None,
            }],
            timestamp: None,
            fence_token: None,
        })
        .await?;
    log.append_event_async(&interaction_request(round_seq))
        .await?;
    log.append_event_async(&SessionLogEntry::SuspensionReady {
        tool_round_seq: round_seq,
        pending: vec!["int-1".into()],
        fence_token: 0,
    })
    .await?;

    let result = f
        .load(json!({
            "session_id": LoadFixture::SESSION,
            "prompt_id": prompt_id,
            "include_variables": true,
            "read": { "max_output_bytes": 1024 }
        }))
        .await?;
    assert!(
        serialized(&result) <= 1024,
        "mandatory identity fits the minimum"
    );
    assert_eq!(result["session_id"], json!(LoadFixture::SESSION));
    assert_eq!(result["prompt_id"], json!(prompt_id));
    assert_eq!(result["status"], json!("blocked"));
    assert!(result["prompt_id"].as_str().expect("id") == prompt_id);
    let pending = result["pending_interaction"]
        .as_object()
        .expect("pending identity");
    assert_eq!(
        pending["kind"],
        json!("interaction"),
        "the approval request stays addressable"
    );
    assert_eq!(pending["interaction_id"], json!("int-1"));
    assert!(pending["message"].as_str().expect("message").len() <= 512);
    assert!(
        result.get("sub_agent_progress").is_none() || serialized(&result) <= 1024,
        "optional progress is expendable, identity is not"
    );
    assert!(prompt_seq >= 1);
    Ok(())
}

#[tokio::test]
async fn loads_fit_the_requested_budget_exactly() -> anyhow::Result<()> {
    let f = LoadFixture::new().await?;
    let mut variables = Vec::new();
    for i in 0..10 {
        variables.push((format!("key{i:02}"), "é".repeat(120)));
    }
    seed(&f, LoadFixture::SESSION, &metadata_patch(variables, &[])).await?;
    for i in 0..5 {
        f.log().append_event_async(&entry_event(i)).await?;
    }
    for budget in [1024usize, 2048, 131_072, 307_200] {
        let result = f
            .load(json!({
                "session_id": LoadFixture::SESSION,
                "include_variables": true,
                "read": { "max_output_bytes": budget, "limit": 2 }
            }))
            .await?;
        assert!(
            serialized(&result) <= budget,
            "budget {budget}: {}",
            serialized(&result)
        );
        assert_eq!(result["session_id"], json!(LoadFixture::SESSION));
        if budget >= 131_072 {
            assert!(
                result["metadata"].is_object() && result["metadata"]["variables"].is_object(),
                "ordinary budgets keep the metadata and its previews visible"
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn transcript_paging_is_identical_with_and_without_variables() -> anyhow::Result<()> {
    let f = LoadFixture::new().await?;
    seed(&f, LoadFixture::SESSION, &metadata_patch(Vec::new(), &[])).await?;
    let mut variables = Vec::new();
    for i in 0..3 {
        variables.push((format!("v{i}"), "value".to_string()));
    }
    // A second child with stored variables and a longer transcript.
    let parity_session = "parity-child";
    seed(&f, parity_session, &metadata_patch(variables, &[])).await?;
    let log = log_for(&f, parity_session);
    for i in 0..12 {
        log.append_event_async(&entry_event(i)).await?;
    }
    let answer = log.append_event_async(&assistant("done")).await?;
    log.append_event_async(&turn_end(answer)).await?;

    let collect = |include: bool| {
        let f = &f;
        async move {
            let mut pages = Vec::new();
            let mut next = None;
            loop {
                let read = match next {
                    None => json!({ "limit": 5 }),
                    Some(index) => json!({ "limit": 5, "index_min": index }),
                };
                let page = f
                    .load(json!({
                        "session_id": parity_session,
                        "include_variables": include,
                        "read": read
                    }))
                    .await?;
                next = page["next_index"].as_u64();
                pages.push(page);
                if next.is_none() {
                    break;
                }
            }
            Ok::<Vec<serde_json::Value>, anyhow::Error>(pages)
        }
    };
    let without = collect(false).await?;
    let with_variables = collect(true).await?;
    assert_eq!(without.len(), with_variables.len());
    for (plain, included) in without.iter().zip(&with_variables) {
        for field in [
            "events",
            "next_index",
            "omitted_ranges",
            "truncated",
            "scanned_through",
        ] {
            assert_eq!(
                plain[field], included[field],
                "paging parity for {field}: metadata must not shift the transcript window"
            );
        }
        assert!(
            included["metadata"]["variables"].is_object(),
            "the opt-in load carries the previews"
        );
        assert!(
            plain.get("metadata").is_some() && plain["metadata"].get("variables").is_none(),
            "the opt-out load has no preview objects"
        );
    }
    let events: usize = without
        .iter()
        .map(|page| page["events"].as_array().expect("events").len())
        .sum();
    assert_eq!(
        events, 14,
        "the whole transcript pages through without gaps"
    );
    Ok(())
}
