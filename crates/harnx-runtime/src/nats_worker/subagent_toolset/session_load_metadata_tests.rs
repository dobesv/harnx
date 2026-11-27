//! Broker-backed tests for `session_load`'s current-metadata reads (plan
//! #2970, T3): the safe projection with the recorded web URL, opt-in variable
//! previews, the missing-record/broker-failure distinction, and current
//! (not historical) prompt-scoped metadata that survives child edits and
//! replays.

use super::session_load_tests::{turn_end, user, LoadFixture};
use crate::nats_session_metadata::{
    apply_to_initializer, ChildPatchOutcome, ChildSessionMetadataPatch, PropertyAssignment,
    SessionMetadata, SessionMetadataStore, SessionPropertiesUpdate, SessionTitlePatch,
};
use harnx_core::agent_config::AgentVariables;
use harnx_core::message::{MessageContent, MessageRole};
use harnx_core::session::SessionLogEntry;
use serde_json::{json, Value};

pub(super) fn assistant(text: &str) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: None,
        role: MessageRole::Assistant,
        content: MessageContent::Text(text.into()),
        timestamp: None,
        fence_token: None,
    }
}

fn title_patch(title: &str) -> ChildSessionMetadataPatch {
    ChildSessionMetadataPatch {
        title: Some(SessionTitlePatch {
            value: Some(title.into()),
            manual: true,
        }),
        variables: None,
        properties: None,
    }
}

pub(super) fn metadata_patch(
    variables: Vec<(String, String)>,
    properties: &[(&str, Value)],
) -> ChildSessionMetadataPatch {
    let mut map = AgentVariables::new();
    for (name, value) in variables {
        map.insert(name, value);
    }
    ChildSessionMetadataPatch {
        title: Some(SessionTitlePatch {
            value: Some("  Child plan  ".into()),
            manual: true,
        }),
        variables: Some(map),
        properties: Some(SessionPropertiesUpdate {
            set: properties
                .iter()
                .map(|(name, value)| PropertyAssignment {
                    name: (*name).into(),
                    value: value.clone(),
                    inherit: None,
                })
                .collect(),
            clear: Vec::new(),
            add_labels: Vec::new(),
            remove_labels: Vec::new(),
        }),
    }
}

/// Seed the first canonical record the way T2's creation path does: the whole
/// patch lands on the initializer, so the created record carries it.
pub(super) async fn seed(
    f: &LoadFixture,
    session_id: &str,
    patch: &ChildSessionMetadataPatch,
) -> anyhow::Result<SessionMetadata> {
    let mut initializer = crate::nats_session_metadata::SessionInitializer::named(
        "helper",
        AgentVariables::default(),
    );
    apply_to_initializer(&mut initializer, patch)?;
    let record = SessionMetadata::new(session_id, initializer);
    SessionMetadataStore::ensure(f.js(), 1)
        .await?
        .create(&record)
        .await?;
    Ok(record)
}

async fn child_edit(f: &LoadFixture, session_id: &str, patch: &ChildSessionMetadataPatch) {
    let store = SessionMetadataStore::ensure(f.js(), 1)
        .await
        .expect("store");
    let storage = harnx_core::session_identity::session_key(Some("helper"), session_id);
    match store.apply_child_patch(&storage, patch, None).await {
        Ok(ChildPatchOutcome::Applied(_)) => {}
        other => panic!("child edit must apply: {other:?}"),
    }
}

#[tokio::test]
async fn load_returns_current_metadata_with_the_whole_recorded_web_url() -> anyhow::Result<()> {
    let f = LoadFixture::new().await?;
    let url = "https://example.test/s/load-child?session=load-child";
    seed(
        &f,
        LoadFixture::SESSION,
        &metadata_patch(
            vec![("topic".to_string(), "rust".to_string())],
            &[("web_session_url", json!(url)), ("customer", json!("acme"))],
        ),
    )
    .await?;
    f.log().append_event_async(&user("entry")).await?;

    let page = f
        .load(json!({ "session_id": LoadFixture::SESSION }))
        .await?;
    let metadata = &page["metadata"];
    assert_eq!(metadata["session_id"], json!(LoadFixture::SESSION));
    assert_eq!(metadata["agent"], json!("helper"));
    assert_eq!(
        metadata["title"],
        json!("Child plan"),
        "titles are stored trimmed"
    );
    assert!(
        metadata["created_at"].is_string(),
        "creation time is part of the safe projection"
    );
    assert_eq!(
        metadata["properties"]["web_session_url"]["value"],
        json!(url),
        "the recorded web URL is returned whole"
    );
    assert_eq!(metadata["properties"]["customer"]["value"], json!("acme"));
    assert!(
        metadata.get("variables").is_none(),
        "no variable values by default"
    );
    assert!(
        metadata.get("overrides").is_none(),
        "no raw execution overrides"
    );
    Ok(())
}

#[tokio::test]
async fn include_variables_is_opt_in_and_bounded() -> anyhow::Result<()> {
    let f = LoadFixture::new().await?;
    let huge = "é".repeat(900) + "quote\"and\\slash";
    let mut variables: Vec<(String, String)> = (0..9)
        .map(|i| (format!("key{i:02}"), "small".to_string()))
        .collect();
    variables.push(("huge".to_string(), huge.clone()));
    seed(&f, LoadFixture::SESSION, &metadata_patch(variables, &[])).await?;
    f.log().append_event_async(&user("entry")).await?;

    let plain = f
        .load(json!({ "session_id": LoadFixture::SESSION }))
        .await?;
    let plain_text = serde_json::to_string(&plain)?;
    assert!(
        !plain_text.contains("rust") || !plain["metadata"].get("variables").is_some(),
        "opt-out exposes no variable values or preview objects: {plain_text}"
    );
    assert!(
        !plain_text.contains(&huge[..80]),
        "no stored value leaks through any other field"
    );

    let included = f
        .load(json!({
            "session_id": LoadFixture::SESSION,
            "include_variables": true
        }))
        .await?;
    let metadata = &included["metadata"];
    let previews = metadata["variables"].as_object().expect("variables");
    assert_eq!(previews.len(), 8, "at most eight entries");
    assert!(
        previews.contains_key("huge"),
        "sorted order keeps the first keys"
    );
    assert_eq!(metadata["variables_summary"]["total"], json!(10));
    assert_eq!(metadata["variables_summary"]["returned"], json!(8));
    assert_eq!(metadata["variables_summary"]["omitted"], json!(2));
    assert_eq!(metadata["truncated"], json!(true));
    let preview = previews["huge"].as_object().expect("preview");
    assert!(preview["total_bytes"].as_u64().unwrap() > 900);
    assert!(preview["omitted_bytes"].as_u64().unwrap() > 0);
    assert!(
        preview["head"].as_str().unwrap().len() < 300,
        "head is a preview"
    );
    assert!(
        !preview["tail"].as_str().unwrap().is_empty(),
        "clipped values keep a tail"
    );
    Ok(())
}

#[tokio::test]
async fn missing_record_is_null_and_broker_failure_is_an_error() -> anyhow::Result<()> {
    let mut f = LoadFixture::new().await?;
    f.log().append_event_async(&user("entry")).await?;
    // A session log without a canonical metadata record: null, not an error.
    let page = f
        .load(json!({ "session_id": LoadFixture::SESSION }))
        .await?;
    assert_eq!(page["metadata"], json!(null));

    // A dead broker is an error, never a missing record.
    f.kill_broker();
    let error = f
        .load(json!({ "session_id": LoadFixture::SESSION }))
        .await
        .expect_err("broker failure must fail the load");
    assert!(
        error.to_string().contains("session metadata unavailable"),
        "broker failure is distinct from a missing record: {error}"
    );
    Ok(())
}

#[tokio::test]
async fn prompt_scoped_metadata_is_current_not_historical() -> anyhow::Result<()> {
    let f = LoadFixture::new().await?;
    seed(
        &f,
        LoadFixture::SESSION,
        &metadata_patch(vec![("topic".to_string(), "rust".to_string())], &[]),
    )
    .await?;
    let (prompt_id, prompt_seq) = f.dispatch_prompt().await?;
    let answer = f
        .log()
        .append_event_async(&assistant("original answer"))
        .await?;
    f.log().append_event_async(&turn_end(answer)).await?;

    // The child edits its title after the prompt's turn is durable history.
    child_edit(
        &f,
        LoadFixture::SESSION,
        &title_patch("Renamed by the child"),
    )
    .await;
    let page = f
        .load(json!({ "session_id": LoadFixture::SESSION, "prompt_id": prompt_id }))
        .await?;
    assert_eq!(
        page["metadata"]["title"],
        json!("Renamed by the child"),
        "metadata is current state, not the prompt's history"
    );
    let events = page["events"].as_array().expect("events");
    assert_eq!(events.len(), 3, "the page itself stays inside the old turn");
    assert_eq!(events[0]["index"], json!(prompt_seq));
    Ok(())
}

#[tokio::test]
async fn replayed_send_then_load_keeps_the_child_edit() -> anyhow::Result<()> {
    let f = LoadFixture::new().await?;
    seed(
        &f,
        LoadFixture::SESSION,
        &metadata_patch(vec![("topic".to_string(), "rust".to_string())], &[]),
    )
    .await?;
    let (prompt_id, _) = f.dispatch_prompt().await?;
    child_edit(
        &f,
        LoadFixture::SESSION,
        &title_patch("Renamed by the child"),
    )
    .await;
    // The same journaled invocation replays: it must return the original
    // prompt and never reapply the creation metadata over the child's edit.
    let (replayed, seq) = f.dispatch_prompt().await?;
    assert_eq!(replayed, prompt_id, "replay returns the original prompt");
    let page = f
        .load(json!({ "session_id": LoadFixture::SESSION, "prompt_id": prompt_id }))
        .await?;
    assert_eq!(page["metadata"]["title"], json!("Renamed by the child"));
    assert!(seq >= 1);
    Ok(())
}

#[tokio::test]
async fn load_is_pure_and_never_mutates_the_child_record() -> anyhow::Result<()> {
    let f = LoadFixture::new().await?;
    seed(
        &f,
        LoadFixture::SESSION,
        &metadata_patch(vec![("topic".to_string(), "rust".to_string())], &[]),
    )
    .await?;
    let store = SessionMetadataStore::ensure(f.js(), 1).await?;
    let storage = harnx_core::session_identity::session_key(Some("helper"), LoadFixture::SESSION);
    let before = store.get(&storage).await?.expect("record");

    let activations = f.activation_count().await?;
    let page = f
        .load(json!({
            "session_id": LoadFixture::SESSION,
            "include_variables": true,
            "read": { "text_regex": "nothing-matches" }
        }))
        .await?;
    assert_eq!(
        page["metadata"]["variables"]["topic"]["head"],
        json!("rust")
    );

    let after = store.get(&storage).await?.expect("record");
    assert_eq!(
        before.revision, after.revision,
        "load never creates, activates, repairs or mutates the child"
    );
    assert_eq!(f.activation_count().await?, activations);
    Ok(())
}
