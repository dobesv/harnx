use super::*;
use anyhow::Context;
use harnx_core::session::ToolOutput;
use harnx_core::session_reconstruct::apply_log_mutations_nats;

pub(super) async fn results(log: &NatsSessionLog) -> Result<Vec<ToolOutput>> {
    let entries = apply_log_mutations_nats(&log.load_events_async().await?)?;
    entries
        .into_iter()
        .find_map(|(_, entry)| match entry {
            SessionLogEntry::ToolResults { results, .. } => Some(results),
            _ => None,
        })
        .context("completed tool results")
}

pub(super) fn assert_results(results: &[ToolOutput]) {
    assert_eq!(results.len(), 3);
    let bad = results
        .iter()
        .find(|r| r.id.as_deref() == Some("bad-call"))
        .unwrap();
    assert_bad_output(&bad.output);
    assert!(
        bad.content.is_empty(),
        "malformed URL must never reach hydration"
    );
    let sibling = results
        .iter()
        .find(|r| r.id.as_deref() == Some("sibling-call"))
        .unwrap();
    assert_eq!(sibling.output, json!({"sibling": "retained"}));
}

fn assert_bad_output(output: &Value) {
    assert_eq!(output["is_error"], true);
    assert_eq!(output["original_output"], tools::malformed_output());
    assert_eq!(
        output["error"],
        "invalid tool image attachment URL 'cid:media:invalid': media URL missing agent segment"
    );
}

pub(super) fn assert_provider_results(request: &Value) {
    let messages = request["messages"].as_array().unwrap();
    for id in ["bad-call", "sibling-call"] {
        let message = messages.iter().find(|m| m["tool_call_id"] == id).unwrap();
        let output: Value = serde_json::from_str(message["content"].as_str().unwrap()).unwrap();
        if id == "bad-call" {
            assert_bad_output(&output);
        } else {
            assert_eq!(output, json!({"sibling": "retained"}));
        }
    }
    assert_eq!(
        messages.last().unwrap()["role"],
        "user",
        "image parts accompany tool results"
    );
}

pub(super) async fn retract_completion(log: &NatsSessionLog) -> Result<()> {
    let entries = log.load_events_async().await?;
    let from = entries
        .iter()
        .find_map(|(seq, entry)| {
            matches!(entry, SessionLogEntry::ToolResults { .. }).then_some(*seq)
        })
        .context("live ToolResults sequence")?;
    let to = entries.last().context("live transcript")?.0;
    log.append_event_async(&SessionLogEntry::EditEntries {
        from: from.try_into()?,
        to: to.try_into()?,
        replacements: vec![],
    })
    .await?;
    let effective = apply_log_mutations_nats(&log.load_events_async().await?)?;
    assert!(effective
        .iter()
        .any(|(_, e)| matches!(e, SessionLogEntry::ToolCalls { .. })));
    assert!(!effective
        .iter()
        .any(|(_, e)| matches!(e, SessionLogEntry::ToolResults { .. })));
    Ok(())
}
