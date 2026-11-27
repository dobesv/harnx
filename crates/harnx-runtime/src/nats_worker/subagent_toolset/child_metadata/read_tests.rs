//! Projection unit tests (plan #2970, T3): the safe metadata view, its
//! deterministic bounds, and the opt-in stored-variable previews. All sizes
//! are asserted against actual compact serialized JSON.

use super::*;
use crate::nats_session_metadata::{
    PropertyAssignment, SessionInitializer, SessionPropertiesUpdate, SESSION_PROPERTIES_NAMESPACE,
};
use crate::nats_worker::subagent_toolset::session_read;
use harnx_core::agent_config::AgentVariables;
use harnx_core::execution_context::{
    ExecutionContextExtension, ExecutionContextObservation, GitRemoteObservation,
    GitRepositoryObservation, EXECUTION_CONTEXT_NAMESPACE,
};
use serde_json::json;

fn metadata(session_id: &str) -> SessionMetadata {
    SessionMetadata::new(
        session_id,
        SessionInitializer::named("helper", AgentVariables::default()),
    )
}

fn set_property(metadata: &mut SessionMetadata, name: &str, value: serde_json::Value) {
    let mut properties = session_properties(metadata).expect("properties decode");
    SessionPropertiesUpdate {
        set: vec![PropertyAssignment {
            name: name.into(),
            value,
            inherit: None,
        }],
        clear: Vec::new(),
        add_labels: Vec::new(),
        remove_labels: Vec::new(),
    }
    .apply(&mut properties)
    .expect("property applies");
    metadata.extensions.insert(
        SESSION_PROPERTIES_NAMESPACE.to_string(),
        serde_json::to_value(&properties).expect("properties serialize"),
    );
}

fn set_repositories(metadata: &mut SessionMetadata, repos: &[(&str, &str)]) {
    let contexts = repos
        .iter()
        .map(|(repository, branch)| ExecutionContextObservation {
            version: 1,
            observed_at: chrono::Utc::now(),
            workspace_root: "/w".into(),
            working_directory: "/w/r".into(),
            repository: Some(GitRepositoryObservation {
                worktree_root: "/w/r".into(),
                branch: Some((*branch).into()),
                remotes: vec![GitRemoteObservation {
                    name: "origin".into(),
                    repository: (*repository).into(),
                    primary: true,
                }],
            }),
            provenance: Some(harnx_core::execution_context::ToolObservationProvenance {
                server_scope: "nats".into(),
                server_identity: "helper".into(),
                tool_name: "bash_exec".into(),
                call_id: "call-1".into(),
                worker_received_at: chrono::Utc::now(),
            }),
        })
        .collect();
    metadata.extensions.insert(
        EXECUTION_CONTEXT_NAMESPACE.to_string(),
        serde_json::to_value(ExecutionContextExtension {
            version: 1,
            contexts,
        })
        .expect("contexts serialize"),
    );
}

fn projection(
    metadata: &SessionMetadata,
    include_variables: bool,
    allowance: usize,
) -> LoadMetadata {
    super::load_metadata(Some(metadata), include_variables, allowance).expect("projection")
}

fn projection_view(metadata: &LoadMetadata) -> &serde_json::Value {
    match metadata {
        LoadMetadata::Bounded(view) => view,
        other => panic!("expected a bounded projection, got {other:?}"),
    }
}

fn serialized(value: &serde_json::Value) -> usize {
    serde_json::to_vec(value).expect("serializes").len()
}

#[test]
fn allowance_is_capped_at_4096_and_a_quarter_of_the_budget() {
    assert_eq!(metadata_allowance(1024), 256);
    assert_eq!(metadata_allowance(8192), 2048);
    assert_eq!(metadata_allowance(100_000), 4096);
    assert_eq!(
        metadata_allowance(session_read::DEFAULT_MAX_OUTPUT_BYTES as usize),
        4096
    );
    assert_eq!(
        metadata_allowance(session_read::MAX_MAX_OUTPUT_BYTES as usize),
        4096
    );
}

#[test]
fn projection_reports_safe_fields_and_no_variables_by_default() {
    let mut record = metadata("child-1");
    record.title.value = Some("Research: rust async".into());
    record.variables.insert("topic".into(), "rust".into());
    set_property(
        &mut record,
        "web_session_url",
        json!("https://example.test/s/child-1"),
    );
    set_property(&mut record, "customer", json!("acme"));
    set_repositories(
        &mut record,
        &[("dobesv/harnx", "main"), ("acme/app", "dev")],
    );

    let metadata = projection(&record, false, 4096);
    let view = projection_view(&metadata);
    assert_eq!(view["session_id"], json!("child-1"));
    assert_eq!(view["agent"], json!("helper"));
    assert_eq!(view["title"], json!("Research: rust async"));
    assert!(view["created_at"].is_string());
    assert_eq!(
        view["properties"]["web_session_url"]["value"],
        json!("https://example.test/s/child-1"),
        "the recorded web URL is returned whole"
    );
    assert_eq!(view["properties"]["customer"]["value"], json!("acme"));
    // Repository contexts never expose private workspace paths.
    assert_eq!(
        view["observed_repositories"],
        json!([
            {"repository": "acme/app", "branch": "dev"},
            {"repository": "dobesv/harnx", "branch": "main"},
        ]),
        "repositories are reported in sorted order"
    );
    assert_eq!(view["truncated"], json!(false));
    assert!(
        view.get("variables").is_none() && view.get("variables_summary").is_none(),
        "no variable values or previews appear without include_variables"
    );
    assert!(
        view.get("overrides").is_none() && view.get("extensions").is_none(),
        "raw internals stay private"
    );
    assert!(
        serialized(&json!({"metadata": view})) <= 4096,
        "the wrapper-inclusive size respects the allowance"
    );
}

#[test]
fn variable_previews_are_sorted_bounded_and_exact() {
    let mut record = metadata("child-1");
    for i in 0..12 {
        record
            .variables
            .insert(format!("key{i:02}"), format!("value {i}"));
    }
    let huge = "é".repeat(1000) + &"\tquote\"back\\slash".repeat(20);
    record.variables.insert("huge".into(), huge.clone());
    record.variables.insert("complete".into(), "small".into());

    let view = projection_view(&projection(&record, true, 4096)).clone();
    let variables = view["variables"].as_object().expect("variables");
    assert_eq!(variables.len(), 8, "at most eight entries");
    let names: Vec<&str> = variables.keys().map(String::as_str).collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "entries are sorted by name");
    assert!(
        names.contains(&"huge") && names.contains(&"key00"),
        "earliest keys win"
    );

    let summary = &view["variables_summary"];
    assert_eq!(summary["total"], json!(14));
    assert_eq!(summary["returned"], json!(8));
    assert_eq!(summary["omitted"], json!(6));
    assert_eq!(
        view["truncated"],
        json!(true),
        "omitted entries mark truncation"
    );

    let complete = &variables["complete"];
    assert_eq!(
        complete["tail"],
        json!(""),
        "a complete value has an empty tail"
    );
    assert_eq!(complete["omitted_bytes"], json!(0));

    let preview = &variables["huge"];
    let head = preview["head"].as_str().expect("head");
    let tail = preview["tail"].as_str().expect("tail");
    let head_json: usize = head.chars().map(session_read::escaped_char_len).sum();
    let tail_json: usize = tail.chars().map(session_read::escaped_char_len).sum();
    assert!(head_json + tail_json <= 256, "escaped content cap");
    assert!(
        head_json >= 100 && tail_json >= 100,
        "clipped values split head and tail"
    );
    let total = preview["total_bytes"].as_u64().expect("total") as usize;
    let omitted = preview["omitted_bytes"].as_u64().expect("omitted") as usize;
    assert_eq!(
        total,
        head.len() + omitted + tail.len(),
        "byte counts refer to original UTF-8 value bytes"
    );
    assert!(
        omitted > 0 && !tail.is_empty(),
        "a clipped value reports both cuts"
    );
}

#[test]
fn variable_keys_and_entries_are_whole_or_omitted() {
    let mut record = metadata("child-1");
    record.variables.insert("a-short-key".into(), "v".into());
    let long_key = "k".repeat(120);
    record.variables.insert(long_key.clone(), "v".into());

    // An allowance that fits the identity core and one entry but not the
    // long key: the long key is omitted whole, the summary says so.
    let view = projection_view(&projection(&record, true, 260)).clone();
    let variables = view["variables"].as_object().expect("variables");
    assert!(variables.contains_key("a-short-key"));
    assert!(
        !variables.contains_key(&long_key) && !variables.keys().any(|k| k.len() > 64),
        "an entry whose name cannot fit is omitted whole"
    );
    assert_eq!(view["variables_summary"]["omitted"], json!(1));
    assert_eq!(view["truncated"], json!(true));
}

#[test]
fn projection_falls_back_to_the_marker_then_to_omission() {
    let mut record = metadata(&"s".repeat(300));
    record.title.value = Some("t".repeat(500));

    // Identity alone fits: the projection is returned with everything else
    // omitted and truncated flagged.
    let view = projection_view(&projection(&record, false, 480)).clone();
    assert_eq!(view["session_id"], json!("s".repeat(300)));
    assert_eq!(view["truncated"], json!(true));
    assert!(
        view.get("title").is_none(),
        "the huge title is omitted, not clipped"
    );

    // Identity cannot fit but the marker can.
    let marker = projection_view(&projection(&record, false, 40)).clone();
    assert_eq!(marker, json!({"truncated": true}));

    // Not even the marker fits: the metadata field is omitted entirely.
    assert!(matches!(
        projection(&record, false, 25),
        LoadMetadata::Omitted
    ));
}

#[test]
fn absent_record_is_null_and_distinct_from_a_bounded_projection() {
    assert!(matches!(
        super::load_metadata(None, true, 4096).expect("projection"),
        LoadMetadata::Absent
    ));
}

#[test]
fn web_url_survives_where_later_properties_are_dropped() {
    let mut record = metadata("child-1");
    let url = format!("https://example.test/s/{}", "x".repeat(90));
    set_property(&mut record, "web_session_url", json!(url.clone()));
    set_property(
        &mut record,
        "zz_late_property",
        json!(serde_json::Value::Null),
    );

    let view = projection_view(&projection(&record, false, 420)).clone();
    assert_eq!(
        view["properties"]["web_session_url"]["value"],
        json!(url),
        "the recorded web URL is whole-or-omit, never clipped"
    );
    assert!(
        !view["properties"]
            .as_object()
            .expect("properties")
            .is_empty(),
        "any property that does not fit is dropped instead"
    );
}

#[test]
fn variable_summary_matches_previews_after_entry_is_evicted() {
    let mut record = metadata("child1");
    record.variables.insert("x".into(), "v".repeat(21));

    let view = projection_view(&projection(&record, true, 256)).clone();
    let variables = view["variables"].as_object().expect("variables");
    let summary = &view["variables_summary"];
    assert_eq!(summary["total"], json!(1));
    assert_eq!(summary["returned"], json!(variables.len()));
    assert_eq!(summary["omitted"], json!(1 - variables.len()));
    assert!(variables.is_empty(), "tight allowance evicts the preview");
    assert_eq!(view["truncated"], json!(true));
    assert!(
        serialized(&json!({"metadata": view})) <= 256,
        "the wrapper-inclusive projection stays within its allowance"
    );
}

#[test]
fn metadata_wrapper_allowance_includes_escaping_and_multibyte_values() {
    for total_budget in [1024, 16_384] {
        let allowance = metadata_allowance(total_budget);
        let mut record = metadata("child1");
        let base = projection_view(&projection(&record, false, 4096)).clone();
        let old_view_budget = allowance - "\"metadata\":".len();

        // Find a title the old key-only reservation would have admitted even
        // though its complete standalone metadata wrapper exceeded allowance.
        let title = (0..=allowance).find_map(|repetitions| {
            (0..=8).find_map(|suffix| {
                let candidate = format!("{}{}", "\"\\éx".repeat(repetitions), "x".repeat(suffix));
                let mut projected = base.as_object().expect("view object").clone();
                projected.insert("title".into(), json!(candidate));
                let view_bytes = serialized(&serde_json::Value::Object(projected));
                (view_bytes <= old_view_budget && view_bytes + "{\"metadata\":}".len() > allowance)
                    .then_some(candidate)
            })
        });
        let title = title.expect("candidate exercises old punctuation undercount");
        record.title.value = Some(title);

        let view = projection_view(&projection(&record, true, allowance)).clone();
        let wrapper_bytes = serialized(&json!({"metadata": view}));
        assert!(
            wrapper_bytes <= allowance,
            "metadata wrapper exceeds {allowance}-byte allowance: {wrapper_bytes}"
        );
        assert!(
            view.get("title").is_none(),
            "over-budget title is omitted whole"
        );
        assert_eq!(view["truncated"], json!(true));
    }
}
