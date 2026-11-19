use super::*;
use crate::interaction_checkpoint::{
    round_start_key, round_started, RoundStart, ROUND_START_BUCKET,
};
use anyhow::Context;

fn calls() -> SessionLogEntry {
    SessionLogEntry::ToolCalls {
        text: String::new(),
        thought: None,
        calls: vec![],
        timestamp: None,
        fence_token: Some(1),
    }
}

fn ready(round: u64) -> SessionLogEntry {
    SessionLogEntry::SuspensionReady {
        tool_round_seq: round,
        pending: vec!["consent".into()],
        fence_token: 1,
    }
}

#[test]
fn clear_abandons_all_earlier_rounds_but_keeps_new_interaction_ownership() {
    let mut entries = vec![(1, calls()), (2, calls()), (3, ready(1))];
    assert!(log_is_parked(&entries));
    entries.push((4, SessionLogEntry::Clear));
    assert!(!log_is_parked(&entries));
    assert!(open_rounds(&entries).is_empty());
    entries.extend([(5, calls()), (6, ready(5))]);
    assert_eq!(open_rounds(&entries), [5]);
    assert!(log_is_parked(&entries));
    entries.extend([(7, SessionLogEntry::Clear), (8, calls())]);
    assert_eq!(open_rounds(&entries), [8]);
    assert!(!log_is_parked(&entries));
}

#[tokio::test]
async fn clear_leaves_checkpoint_only_rounds_inert_and_guards_post_clear_rounds() -> Result<()> {
    let (url, _broker, _dir) = crate::nats_worker::tests::spawn_test_nats()
        .await
        .context("NATS test binary is required")?;
    let js = jetstream::new(async_nats::connect(url).await?);
    let store = js
        .create_key_value(jetstream::kv::Config {
            bucket: ROUND_START_BUCKET.into(),
            ..Default::default()
        })
        .await?;
    let storage_key = "clear-checkpoint-guard";
    for round in [1, 4] {
        let record = RoundStart {
            version: 1,
            session_storage_key: storage_key.into(),
            tool_round_seq: round,
            fence_token: 1,
        };
        store
            .put(
                round_start_key(storage_key, round),
                serde_json::to_vec(&record)?.into(),
            )
            .await?;
    }
    let mut entries = vec![(1, calls())];
    assert!(is_parked(&js, storage_key, &entries).await?);
    entries.extend([(2, SessionLogEntry::Clear), (3, calls())]);
    assert!(!is_parked(&js, storage_key, &entries).await?);
    assert!(
        round_started(&js, storage_key, 1).await?,
        "old records aren't deleted by Clear"
    );
    entries.push((4, calls()));
    assert!(is_parked(&js, storage_key, &entries).await?);
    Ok(())
}

#[test]
fn clear_abandons_selected_handoff_without_hiding_later_handoffs() {
    let handoff = SessionLogEntry::ToolResults {
        results: vec![harnx_core::session::ToolOutput {
            user_surfaces: Vec::new(),
            id: Some("handoff".into()),
            name: "target_session_handoff".into(),
            output: serde_json::json!({}),
            markdown: None,
            content: vec![],
            switch_agent: Some(harnx_core::tool::SwitchAgentData {
                agent: "target".into(),
                prompt: "continue".into(),
                session_id: None,
                tool_call_id: Some("handoff".into()),
            }),
        }],
        handoff_selection: Some("handoff".into()),
        timestamp: None,
    };
    let mut entries = vec![(1, handoff.clone())];
    assert_eq!(pending_handoff(&entries), Some(1));
    entries.push((2, SessionLogEntry::Clear));
    assert_eq!(pending_handoff(&entries), None);
    assert!(!log_is_parked(&entries));
    entries.push((3, handoff));
    assert_eq!(pending_handoff(&entries), Some(3));
}
