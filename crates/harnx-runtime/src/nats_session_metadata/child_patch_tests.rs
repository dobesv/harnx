use super::super::{
    session_properties, ChildSessionMetadataPatch as Patch, SessionInitializer, SessionMetadata,
    SessionProperties,
};
use super::{already_holds, apply_to_initializer, apply_to_metadata};
use harnx_core::agent_config::AgentVariables;
use harnx_core::require_nextest;
use serde_json::{json, Value};

fn named_initializer() -> SessionInitializer {
    SessionInitializer::named("metis", AgentVariables::default())
}

fn custom_properties(count: usize) -> SessionProperties {
    let properties: serde_json::Map<String, Value> = (0..count)
        .map(|index| {
            (
                format!("c{index:02}"),
                json!({"value": format!("v{index}"), "inherit": false}),
            )
        })
        .collect();
    serde_json::from_value(json!(properties)).unwrap()
}

fn metadata_with_initializer(initializer: SessionInitializer) -> SessionMetadata {
    SessionMetadata::new("child-1", initializer)
}

fn title(value: Option<&str>, manual: bool, tokens: usize) -> super::super::SessionTitle {
    super::super::SessionTitle {
        value: value.map(str::to_string),
        manual,
        last_updated_tokens: tokens,
    }
}

#[test]
fn creation_patch_lands_complete_in_the_first_canonical_record() {
    require_nextest();
    let parent = SessionInitializer::named("parent", Default::default()).with_user_id("p-o");
    let mut initializer = SessionInitializer::named("child", AgentVariables::default())
        .with_properties(parent.properties.inherited());
    let patch: Patch = serde_json::from_value(json!({
        "title": {"value": "  Fix the login flow  ", "manual": true},
        "variables": {"priority": "high"},
        "properties": {
            "set": [
                {"name": "git_branch", "value": "dobesv/feature"},
                {"name": "github_issue", "value": 2970},
                {"name": "squad", "value": "core"}
            ],
            "add_labels": ["metadata"]
        }
    }))
    .unwrap();
    apply_to_initializer(&mut initializer, &patch).unwrap();

    let metadata = metadata_with_initializer(initializer);
    metadata.validate("child-1").unwrap();
    assert_eq!(metadata.title.value.as_deref(), Some("Fix the login flow"));
    assert!(metadata.title.manual);
    assert_eq!(
        metadata.variables,
        AgentVariables::from_iter([("priority".to_string(), "high".to_string())])
    );
    let properties = session_properties(&metadata).unwrap();
    assert_eq!(properties.text("git_branch"), Some("dobesv/feature"));
    assert_eq!(properties.get("github_issue").unwrap().value, json!(2970));
    assert_eq!(properties.text("squad"), Some("core"));
    assert_eq!(
        properties.get(super::super::LABELS_PROPERTY).unwrap().value,
        json!(["metadata"])
    );
    // Inherited identity survives the patch untouched.
    assert_eq!(properties.text("user_id"), Some("p-o"));
}

#[test]
fn creation_patch_preserves_a_trusted_creation_patch_except_where_it_supplies() {
    require_nextest();
    let mut initializer = named_initializer().with_metadata_patch(
        serde_json::from_value(json!({
            "title": {"value": "serve title"},
            "overrides": {"model": "m1"}
        }))
        .unwrap(),
    );
    let patch: Patch = serde_json::from_value(json!({
        "title": {"value": "parent title"},
        "variables": {"k": "v"}
    }))
    .unwrap();
    apply_to_initializer(&mut initializer, &patch).unwrap();

    let metadata = metadata_with_initializer(initializer);
    assert_eq!(metadata.title.value.as_deref(), Some("parent title"));
    assert_eq!(metadata.variables.get("k").map(String::as_str), Some("v"));
    // The child input cannot touch execution settings: the trusted creation
    // patch's overrides survive.
    assert_eq!(metadata.overrides.model.as_deref(), Some("m1"));
}

#[test]
fn omitted_variables_are_preserved_and_a_supplied_map_replaces_wholesale() {
    require_nextest();
    let base = || {
        SessionMetadata::new(
            "child-1",
            SessionInitializer::named(
                "metis",
                AgentVariables::from_iter([("keep".to_string(), "base".to_string())]),
            ),
        )
    };

    // Omission preserves the stored variables, on creation and on update.
    let mut initializer = named_initializer();
    let patch: Patch = serde_json::from_value(json!({"title": {"value": "t"}})).unwrap();
    apply_to_initializer(&mut initializer, &patch).unwrap();
    assert!(initializer.variables.is_empty());

    let mut metadata = base();
    let omitted: Patch =
        serde_json::from_value(json!({"properties": {"add_labels": ["x"]}})).unwrap();
    apply_to_metadata(&mut metadata, &omitted).unwrap();
    assert_eq!(
        metadata.variables.get("keep").map(String::as_str),
        Some("base")
    );

    // A supplied map replaces wholesale; an explicit empty map clears.
    let replace: Patch = serde_json::from_value(json!({"variables": {"next": "1"}})).unwrap();
    apply_to_metadata(&mut metadata, &replace).unwrap();
    assert_eq!(metadata.variables.len(), 1);
    assert_eq!(
        metadata.variables.get("next").map(String::as_str),
        Some("1")
    );

    let clear: Patch = serde_json::from_value(json!({"variables": {}})).unwrap();
    apply_to_metadata(&mut metadata, &clear).unwrap();
    assert!(metadata.variables.is_empty());
}

#[test]
fn title_trim_clear_and_manual_semantics_match_the_patch_path() {
    require_nextest();
    let mut metadata = metadata_with_initializer(named_initializer());
    metadata.title = title(Some("Old"), true, 5);

    let trim: Patch = serde_json::from_value(json!({"title": {"value": "  New  "}})).unwrap();
    apply_to_metadata(&mut metadata, &trim).unwrap();
    assert_eq!(metadata.title.value.as_deref(), Some("New"));
    assert!(!metadata.title.manual);

    for blank in [serde_json::Value::Null, json!("  "), json!("")] {
        let clear: Patch =
            serde_json::from_value(json!({"title": {"value": blank, "manual": true}})).unwrap();
        apply_to_metadata(&mut metadata, &clear).unwrap();
        assert_eq!(metadata.title.value, None);
        assert!(!metadata.title.manual);
        assert_eq!(metadata.title.last_updated_tokens, 0);
    }

    // No title in the patch preserves value, manual flag and token stamp.
    let mut manual = metadata_with_initializer(named_initializer());
    manual.title = title(Some("Kept"), true, 100);
    let untouched: Patch = serde_json::from_value(json!({"variables": {"a": "1"}})).unwrap();
    apply_to_metadata(&mut manual, &untouched).unwrap();
    assert_eq!(manual.title.value.as_deref(), Some("Kept"));
    assert!(manual.title.manual);
    assert_eq!(manual.title.last_updated_tokens, 100);
}

#[test]
fn strict_input_rejects_overrides_extensions_and_unknown_fields() {
    require_nextest();
    for rejected in [
        json!({"overrides": {"model": "m1"}}),
        json!({"extensions": {"dev.harnx.session_properties": {}}}),
        json!({"variables": {"a": "v"}, "unknown": 1}),
        json!({"variables": {"a": 3}}),
    ] {
        assert!(
            serde_json::from_value::<Patch>(rejected.clone()).is_err(),
            "expected rejection: {rejected}"
        );
    }
    let accepted: Patch = serde_json::from_value(json!({
        "title": {"value": "t"},
        "variables": {"a": "v"},
        "properties": {"set": [{"name": "git_branch", "value": "b"}]}
    }))
    .unwrap();
    accepted.validate().unwrap();
}

#[test]
fn validate_rejects_read_only_and_non_inheritable_property_operations() {
    require_nextest();
    for rejected in [
        json!({"set": [{"name": "user_id", "value": "u"}]}),
        json!({"set": [{"name": "user_principals", "value": {"user_id": "u"}}]}),
        json!({"clear": ["user_id"]}),
        json!({"set": [{"name": "web_session_url", "value": "https://x/y", "inherit": true}]}),
    ] {
        let patch: Patch = serde_json::from_value(json!({"properties": rejected})).unwrap();
        assert!(patch.validate().is_err(), "expected rejection: {rejected}");
    }
    // The writable, never-inherited web_session_url is fine without inherit.
    let writable: Patch = serde_json::from_value(json!({
        "properties": {"set": [{"name": "web_session_url", "value": "https://x.example/y"}]}
    }))
    .unwrap();
    writable.validate().unwrap();
}

/// A patch that passes every property rule can still overflow the 64 KiB
/// extension namespace a canonical record allows the properties to occupy:
/// 32 custom values of 2040 bytes are individually and count-wise legal, but
/// together they serialize past the namespace limit.
fn overflowing_properties() -> serde_json::Value {
    let set = (0..super::super::CUSTOM_PROPERTIES_MAX)
        .map(|index| json!({"name": format!("c{index:02}"), "value": "x".repeat(2040)}))
        .collect::<Vec<_>>();
    json!({"properties": {"set": set}})
}

#[test]
fn creation_patch_rejects_extensions_past_the_namespace_limit() {
    require_nextest();
    let patch: Patch = serde_json::from_value(overflowing_properties()).unwrap();
    // Per-property rules pass; only the serialized extension size rejects.
    patch.validate().unwrap();

    let mut initializer = named_initializer();
    let before = initializer.clone();
    let error = apply_to_initializer(&mut initializer, &patch)
        .expect_err("over-limit extension must be rejected before reservation");
    assert!(
        error.to_string().contains("exceeds"),
        "unexpected error: {error:#}"
    );
    assert_eq!(initializer, before);
}

#[test]
fn creation_patch_accepts_properties_within_the_namespace_limit() {
    require_nextest();
    let set = (0..16)
        .map(|index| json!({"name": format!("c{index:02}"), "value": "x".repeat(2040)}))
        .collect::<Vec<_>>();
    let patch: Patch = serde_json::from_value(json!({"properties": {"set": set}})).unwrap();
    let mut initializer = named_initializer();
    apply_to_initializer(&mut initializer, &patch)
        .expect("16 x 2040-byte values stay well under the namespace limit");
    // The first record built from it validates, exactly what create will run.
    let metadata = metadata_with_initializer(initializer);
    metadata
        .validate_storage_key(&metadata.storage_key())
        .unwrap();
}

#[test]
fn property_limits_apply_against_inherited_and_current_state() {
    require_nextest();
    // Creation: the count check runs against the initializer's actual
    // (inherited) properties, not a placeholder.
    let mut initializer = SessionInitializer::named("child", Default::default())
        .with_properties(custom_properties(super::super::CUSTOM_PROPERTIES_MAX));
    let over: Patch = serde_json::from_value(json!({
        "properties": {"set": [{"name": "overflow", "value": "v"}]}
    }))
    .unwrap();
    let before = initializer.clone();
    assert!(apply_to_initializer(&mut initializer, &over).is_err());
    assert_eq!(initializer, before);

    // Existing record: the same limit against stored properties.
    let mut metadata = metadata_with_initializer(
        SessionInitializer::named("child", Default::default())
            .with_properties(custom_properties(super::super::CUSTOM_PROPERTIES_MAX)),
    );
    let over: Patch = serde_json::from_value(json!({
        "title": {"value": "valid title"},
        "properties": {"set": [{"name": "overflow", "value": "v"}]}
    }))
    .unwrap();
    let before = metadata.clone();
    assert!(apply_to_metadata(&mut metadata, &over).is_err());
    assert_eq!(metadata, before);

    // Clearing one makes room for exactly one more.
    let swap: Patch = serde_json::from_value(json!({
        "properties": {"clear": ["c00"], "set": [{"name": "fresh", "value": "v"}]}
    }))
    .unwrap();
    apply_to_metadata(&mut metadata, &swap).unwrap();
    assert!(session_properties(&metadata).unwrap().get("c00").is_none());
    assert_eq!(
        session_properties(&metadata).unwrap().text("fresh"),
        Some("v")
    );
}

#[test]
fn invalid_mixed_updates_change_nothing() {
    require_nextest();
    let mut metadata = metadata_with_initializer(named_initializer());
    let before = metadata.clone();
    let mixed: Patch = serde_json::from_value(json!({
        "title": {"value": "would-be title"},
        "variables": {"k": "v"},
        "properties": {"set": [{"name": "github_issue", "value": "not-a-number"}]}
    }))
    .unwrap();
    assert!(apply_to_metadata(&mut metadata, &mixed).is_err());
    assert_eq!(metadata, before);
}

#[test]
fn already_holds_requires_the_complete_normalized_state() {
    require_nextest();
    let requested: Patch = serde_json::from_value(json!({
        "title": {"value": "Target", "manual": true},
        "variables": {"a": "1"},
        "properties": {"set": [{"name": "git_branch", "value": "b"}]}
    }))
    .unwrap();

    let mut metadata = metadata_with_initializer(named_initializer());
    apply_to_metadata(&mut metadata, &requested).unwrap();
    assert!(already_holds(&metadata, &requested));
    // A later child edit (title, manual flag, extra variable) breaks it, so a
    // retried invocation cannot mistake the record for its own work.
    metadata.title.value = Some("Child edit".to_string());
    assert!(!already_holds(&metadata, &requested));
    metadata.title.value = Some("Target".to_string());
    metadata.title.manual = false;
    assert!(!already_holds(&metadata, &requested));
    metadata.title.manual = true;
    metadata
        .variables
        .insert("extra".to_string(), "2".to_string());
    assert!(!already_holds(&metadata, &requested));

    // An empty patch holds vacuously, matching apply's no-write result.
    let empty = Patch::default();
    apply_to_metadata(&mut metadata, &empty).unwrap();
    assert!(already_holds(&metadata, &empty));
    assert!(empty.is_empty());
}
