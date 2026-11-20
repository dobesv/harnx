use super::*;
use serde_json::json;

fn update(value: Value) -> SessionPropertiesUpdate {
    serde_json::from_value(value).expect("valid update")
}

fn applied(properties: &mut SessionProperties, value: Value) -> Result<bool> {
    update(value).apply(properties)
}

fn properties(value: Value) -> SessionProperties {
    serde_json::from_value(value).expect("valid properties")
}

fn error(value: Value) -> String {
    format!(
        "{:#}",
        applied(&mut SessionProperties::default(), value).expect_err("update must fail")
    )
}

#[test]
fn definitions_have_unique_lowercase_names() {
    let mut names = std::collections::BTreeSet::new();
    for definition in PROPERTY_DEFINITIONS {
        assert!(is_custom_name(definition.name), "{}", definition.name);
        assert!(names.insert(definition.name), "{} repeats", definition.name);
        assert!(!definition.description.is_empty());
    }
    let read_only: Vec<_> = PROPERTY_DEFINITIONS
        .iter()
        .filter(|definition| definition.read_only)
        .map(|definition| definition.name)
        .collect();
    assert_eq!(read_only, ["user_id", "user_principals"]);
}

#[test]
fn stores_each_property_with_its_value_and_inherit_flag() {
    let mut properties = SessionProperties::default();
    let changed = applied(
        &mut properties,
        json!({"set": [
            {"name": "github_owner_repo", "value": "dobesv/harnx"},
            {"name": "github_issue", "value": 2296},
            {"name": "web_session_url", "value": "https://harnx.example/agents/a/sessions/s"},
        ]}),
    )
    .unwrap();
    assert!(changed);
    assert_eq!(
        serde_json::to_value(&properties).unwrap(),
        json!({
            "github_owner_repo": {"value": "dobesv/harnx", "inherit": true},
            "github_issue": {"value": 2296, "inherit": true},
            "web_session_url": {
                "value": "https://harnx.example/agents/a/sessions/s",
                "inherit": false
            },
        })
    );
}

#[test]
fn numbers_accept_issue_references_and_zero_removes_them() {
    let mut properties = SessionProperties::default();
    applied(
        &mut properties,
        json!({"set": [
            {"name": "github_issue", "value": "#2296"},
            {"name": "github_pull_request", "value": " 2300 "},
        ]}),
    )
    .unwrap();
    assert_eq!(properties.get("github_issue").unwrap().value, json!(2296));
    assert_eq!(
        properties.get("github_pull_request").unwrap().value,
        json!(2300)
    );

    applied(
        &mut properties,
        json!({"set": [{"name": "github_issue", "value": 0}]}),
    )
    .unwrap();
    assert!(properties.get("github_issue").is_none());

    for value in [json!(-1), json!(1.5), json!("issue 7"), json!([7])] {
        let message = error(json!({"set": [{"name": "github_issue", "value": value}]}));
        assert!(
            message.contains("github_issue must be a positive integer"),
            "{message}"
        );
    }
}

#[test]
fn well_known_text_properties_are_validated() {
    for (name, value, requirement) in [
        ("github_owner_repo", "harnx", "look like owner/repo"),
        (
            "github_owner_repo",
            "https://github.com/dobesv/harnx",
            "owner/repo",
        ),
        ("github_owner_repo", "dobesv/../harnx", "owner/repo"),
        ("git_branch", "has space", "branch name"),
        ("git_branch", "line\nbreak", "branch name"),
        (
            "external_task_url",
            "ftp://tracker.example/1",
            "http or https URL",
        ),
        (
            "external_task_url",
            "https:/tracker.example/1",
            "http or https URL",
        ),
        (
            "web_session_url",
            "https://user:secret@harnx.example/",
            "without credentials",
        ),
        ("working_directory", "/srv/\u{7}bell", "control characters"),
    ] {
        let message = error(json!({"set": [{"name": name, "value": value}]}));
        assert!(
            message.contains(&format!("{name} must")) && message.contains(requirement),
            "{name}={value:?}: {message}"
        );
    }
    for value in [json!(7), json!(["dobesv/harnx"])] {
        let message = error(json!({"set": [{"name": "github_owner_repo", "value": value}]}));
        assert!(message.contains("github_owner_repo must"), "{message}");
    }
}

#[test]
fn empty_values_remove_properties() {
    let mut properties = properties(json!({
        "git_branch": {"value": "main", "inherit": true},
        "external_task_url": {"value": "https://tracker.example/1", "inherit": true},
        "labels": {"value": ["a"], "inherit": false},
        "customer": {"value": "acme", "inherit": true},
    }));
    let changed = applied(
        &mut properties,
        json!({"set": [
            {"name": "git_branch", "value": "  "},
            {"name": "external_task_url", "value": null},
            {"name": "labels", "value": []},
            {"name": "customer"},
        ]}),
    )
    .unwrap();
    assert!(changed);
    assert!(properties.is_empty(), "{properties:?}");
}

#[test]
fn clear_runs_before_set() {
    let mut properties = properties(json!({
        "git_branch": {"value": "main", "inherit": false},
        "working_directory": {"value": "/srv/app", "inherit": true},
    }));
    applied(
        &mut properties,
        json!({
            "clear": ["git_branch", "working_directory"],
            "set": [{"name": "git_branch", "value": "feature"}],
        }),
    )
    .unwrap();
    // Cleared first, so the branch is new and takes the default flag again.
    assert_eq!(
        serde_json::to_value(&properties).unwrap(),
        json!({"git_branch": {"value": "feature", "inherit": true}})
    );
}

#[test]
fn user_id_is_read_only() {
    for update in [
        json!({"set": [{"name": "user_id", "value": "alice"}]}),
        json!({"clear": ["user_id"]}),
    ] {
        let message = error(update);
        assert!(message.contains("user_id is set by Harnx"), "{message}");
    }
}

#[test]
fn user_principals_are_read_only() {
    // I1: no client, agent or model tool can set or change the snapshot.
    for update in [
        json!({"set": [{
            "name": "user_principals",
            "value": {"user_id": "mallory", "users": ["mallory"], "groups": ["admins"], "roles": []}
        }]}),
        json!({"set": [{"name": "user_principals", "value": null}]}),
        json!({"clear": ["user_principals"]}),
    ] {
        let message = error(update);
        assert!(
            message.contains("user_principals is set by Harnx"),
            "{message}"
        );
    }
}

#[test]
fn user_principals_round_trip_through_metadata() {
    // I5: the snapshot travels in the session-properties extension, so the
    // existing reader keeps records with it, and records without it load.
    let principals = harnx_core::user_principals::from_resolved(
        Some("alice"),
        Some(&["alice".into(), "bob".into()]),
        &["engineering".into()],
        &["reviewer".into()],
    );
    let initializer = super::super::SessionInitializer::named("metis", Default::default())
        .with_user_principals(&principals);
    assert!(
        initializer
            .properties
            .get(USER_PRINCIPALS_PROPERTY)
            .unwrap()
            .inherit
    );

    let metadata = SessionMetadata::new("session-1", initializer);
    assert_eq!(
        session_properties(&metadata)
            .unwrap()
            .get(USER_PRINCIPALS_PROPERTY)
            .unwrap()
            .value,
        principals.to_property_value()
    );
    assert_eq!(user_principals(&metadata), principals);

    // A metadata record carrying the property survives a JSON round trip,
    // and one written before the property existed still loads.
    for expected in [Some(principals.clone()), None] {
        let mut record = SessionMetadata::new(
            "session-1",
            super::super::SessionInitializer::named("metis", Default::default()),
        );
        if let Some(principals) = &expected {
            let mut properties = session_properties(&record).unwrap();
            properties.put(
                USER_PRINCIPALS_PROPERTY,
                principals.to_property_value(),
                None,
            );
            store_properties(&mut record, &properties).unwrap();
        }
        let bytes = serde_json::to_vec(&record).unwrap();
        let decoded: SessionMetadata = serde_json::from_slice(&bytes).unwrap();
        decoded.validate("session-1").unwrap();
        assert_eq!(
            user_principals(&decoded),
            expected.unwrap_or_else(harnx_core::user_principals::anonymous)
        );
    }
}

#[test]
fn maximum_combined_user_principals_fit_session_properties_validation() {
    let long = "\0".repeat(harnx_core::user_principals::USER_PRINCIPALS_MAX_TEXT);
    let max_list = vec![long.clone(); harnx_core::user_principals::USER_PRINCIPALS_MAX_LIST];
    let principals = harnx_core::user_principals::from_resolved(
        Some(&long),
        Some(&max_list),
        &max_list,
        &max_list,
    );
    let initializer = super::super::SessionInitializer::named("metis", Default::default())
        .with_user_principals(&principals);
    let metadata = SessionMetadata::new("maximum-principals", initializer);

    metadata
        .validate("maximum-principals")
        .expect("maximum combined principals fit session-properties namespace");
}

#[test]
fn user_principals_accessor_falls_back_to_the_stored_owner() {
    // Legacy sessions: no snapshot, so the owner id alone decides.
    let legacy =
        super::super::SessionInitializer::named("metis", Default::default()).with_user_id("owner");
    let metadata = SessionMetadata::new("legacy", legacy);
    assert_eq!(
        user_principals(&metadata),
        harnx_core::user_principals::legacy_fallback(Some("owner"))
    );

    // No identity at all reads as anonymous.
    let anonymous = SessionMetadata::new(
        "anon",
        super::super::SessionInitializer::named("metis", Default::default()),
    );
    assert_eq!(
        user_principals(&anonymous),
        harnx_core::user_principals::anonymous()
    );

    // A stored value beats the owner fallback, and a corrupt one falls back.
    let mut properties = SessionProperties::default();
    properties.put(
        USER_PRINCIPALS_PROPERTY,
        json!({"user_id": "alice", "users": ["alice"], "groups": ["a"], "roles": []}),
        None,
    );
    assert_eq!(
        properties.user_principals().user_id.as_deref(),
        Some("alice")
    );
    let mut corrupt = properties;
    corrupt.put(USER_PRINCIPALS_PROPERTY, json!("not-an-object"), None);
    assert_eq!(
        corrupt.user_principals(),
        harnx_core::user_principals::anonymous()
    );
}

#[test]
fn malformed_snapshot_user_id_uses_only_the_stored_owner() {
    let mut metadata = SessionMetadata::new(
        "malformed-snapshot",
        super::super::SessionInitializer::named("metis", Default::default()).with_user_id("owner"),
    );
    let mut properties = session_properties(&metadata).unwrap();
    properties.put(
        USER_PRINCIPALS_PROPERTY,
        json!({
            "user_id": 42,
            "users": ["snapshot-alias"],
            "groups": ["engineering"],
            "roles": ["reviewer"],
        }),
        None,
    );
    store_properties(&mut metadata, &properties).unwrap();
    let stored: SessionMetadata =
        serde_json::from_value(serde_json::to_value(metadata).unwrap()).unwrap();

    assert_eq!(
        user_principals(&stored),
        harnx_core::user_principals::legacy_fallback(Some("owner")),
        "malformed snapshot must not retain aliases, groups, or roles"
    );
}

#[test]
fn corrupt_session_properties_extension_returns_anonymous_principals() {
    let mut metadata = SessionMetadata::new(
        "corrupt-properties",
        super::super::SessionInitializer::named("metis", Default::default()),
    );
    metadata
        .extensions
        .insert(SESSION_PROPERTIES_NAMESPACE.into(), json!("not-a-map"));

    assert_eq!(
        user_principals(&metadata),
        harnx_core::user_principals::anonymous()
    );
}

#[test]
fn inherited_identity_keeps_owner_and_principals_only() {
    // I2: handoff children copy the snapshot verbatim, with no execution
    // context and no recomputation.
    let parent = properties(json!({
        "user_id": {"value": "alice", "inherit": true},
        "user_principals": {
            "value": {"user_id": "alice", "users": ["alice", "bob"], "groups": ["g"], "roles": ["r"]},
            "inherit": true
        },
        "git_branch": {"value": "main", "inherit": true},
        "web_session_url": {"value": "https://harnx.example/", "inherit": false},
    }));
    let inherited = parent.inherited_identity();
    assert_eq!(
        serde_json::to_value(&inherited).unwrap(),
        json!({
            "user_id": {"value": "alice", "inherit": true},
            "user_principals": {
                "value": {"user_id": "alice", "users": ["alice", "bob"], "groups": ["g"], "roles": ["r"]},
                "inherit": true
            },
        })
    );
    assert_eq!(
        inherited.user_principals(),
        harnx_core::user_principals::from_resolved(
            Some("alice"),
            Some(&["alice".into(), "bob".into()]),
            &["g".into()],
            &["r".into()],
        )
    );

    // A snapshot recorded with inherit disabled (a foreign record) is not copied.
    let foreign = properties(json!({
        "user_id": {"value": "alice", "inherit": true},
        "user_principals": {
            "value": {"user_id": "alice", "users": ["alice"], "groups": [], "roles": []},
            "inherit": false
        },
    }));
    assert_eq!(
        serde_json::to_value(foreign.inherited_identity()).unwrap(),
        json!({"user_id": {"value": "alice", "inherit": true}})
    );
}

#[test]
fn custom_properties_hold_text_and_reject_bad_names() {
    let mut properties = SessionProperties::default();
    applied(
        &mut properties,
        json!({"set": [
            {"name": "customer", "value": " acme "},
            {"name": "jira.priority", "value": 2},
            {"name": "release-train", "value": true, "inherit": true},
            {"name": "notes", "value": "line one\n\tline two"},
        ]}),
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(&properties).unwrap(),
        json!({
            "customer": {"value": "acme", "inherit": false},
            "jira.priority": {"value": "2", "inherit": false},
            "notes": {"value": "line one\n\tline two", "inherit": false},
            "release-train": {"value": "true", "inherit": true},
        })
    );

    for name in ["Customer", "9lives", "has space", "", "_private"] {
        let message = error(json!({"set": [{"name": name, "value": "x"}]}));
        assert!(
            message.contains("custom property names"),
            "{name}: {message}"
        );
    }
    for value in [json!(["a"]), json!({"a": 1}), json!("bell\u{7}")] {
        let message = error(json!({"set": [{"name": "customer", "value": value}]}));
        assert!(
            message.contains("custom property customer must"),
            "{message}"
        );
    }
    let message = error(json!({"set": [{"name": "customer", "value": "x".repeat(2049)}]}));
    assert!(
        message.contains("custom property customer must"),
        "{message}"
    );
}

#[test]
fn custom_property_count_is_bounded() {
    let set: Vec<_> = (0..=CUSTOM_PROPERTIES_MAX)
        .map(|index| json!({"name": format!("custom{index}"), "value": "x"}))
        .collect();
    let message = error(json!({ "set": set }));
    assert!(message.contains("custom properties"), "{message}");
}

#[test]
fn labels_are_edited_like_a_set() {
    let mut properties = SessionProperties::default();
    applied(
        &mut properties,
        json!({"add_labels": [" bug ", "needs-review", "bug", ""]}),
    )
    .unwrap();
    assert_eq!(
        properties.get("labels").unwrap(),
        &SessionProperty {
            value: json!(["bug", "needs-review"]),
            inherit: false,
            source: None,
            other: BTreeMap::new(),
        }
    );

    applied(
        &mut properties,
        json!({"remove_labels": ["needs-review"], "add_labels": ["in-review"]}),
    )
    .unwrap();
    assert_eq!(
        properties.get("labels").unwrap().value,
        json!(["bug", "in-review"])
    );

    // Replacing the whole list: clear first, then add.
    applied(
        &mut properties,
        json!({"clear": ["labels"], "add_labels": ["done"]}),
    )
    .unwrap();
    assert_eq!(properties.get("labels").unwrap().value, json!(["done"]));

    applied(&mut properties, json!({"remove_labels": ["done"]})).unwrap();
    assert!(properties.get("labels").is_none());

    let many: Vec<_> = (0..=SESSION_LABELS_MAX)
        .map(|index| format!("label-{index}"))
        .collect();
    let message = error(json!({ "add_labels": many }));
    assert!(message.contains("labels must"), "{message}");
}

#[test]
fn inherit_flag_is_kept_unless_the_writer_sets_it() {
    let mut properties = SessionProperties::default();
    applied(
        &mut properties,
        json!({"set": [
            {"name": "git_branch", "value": "main", "inherit": false},
            {"name": "customer", "value": "acme", "inherit": true},
        ]}),
    )
    .unwrap();
    applied(
        &mut properties,
        json!({"set": [
            {"name": "git_branch", "value": "feature"},
            {"name": "customer", "value": "globex"},
        ]}),
    )
    .unwrap();
    assert!(!properties.get("git_branch").unwrap().inherit);
    assert!(properties.get("customer").unwrap().inherit);

    let changed = applied(
        &mut properties,
        json!({"set": [{"name": "git_branch", "value": "feature", "inherit": true}]}),
    )
    .unwrap();
    assert!(changed, "changing only the flag is a change");
    assert!(properties.get("git_branch").unwrap().inherit);
}

#[test]
fn unchanged_values_report_no_change() {
    let mut properties = properties(json!({
        "git_branch": {"value": "main", "inherit": true},
        "labels": {"value": ["bug"], "inherit": false},
    }));
    let changed = applied(
        &mut properties,
        json!({
            "set": [{"name": "git_branch", "value": " main "}],
            "add_labels": ["bug"],
            "clear": ["github_issue"],
        }),
    )
    .unwrap();
    assert!(!changed);
}

#[test]
fn unknown_attributes_survive_a_rewrite() {
    let mut properties = properties(json!({
        "git_branch": {"value": "main", "inherit": true, "set_by": "worker"},
        "future_property": {"value": {"nested": true}, "inherit": false},
    }));
    applied(
        &mut properties,
        json!({"set": [{"name": "git_branch", "value": "feature"}]}),
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(&properties).unwrap(),
        json!({
            "git_branch": {"value": "feature", "inherit": true, "set_by": "worker"},
            "future_property": {"value": {"nested": true}, "inherit": false},
        })
    );
}

#[test]
fn inherited_keeps_only_flagged_properties() {
    let properties = properties(json!({
        "github_issue": {"value": 2296, "inherit": true},
        "web_session_url": {"value": "https://harnx.example/", "inherit": false},
        "customer": {"value": "acme", "inherit": true},
    }));
    assert_eq!(
        serde_json::to_value(properties.inherited()).unwrap(),
        json!({
            "github_issue": {"value": 2296, "inherit": true},
            "customer": {"value": "acme", "inherit": true},
        })
    );
}

#[test]
fn update_rejects_unknown_parameters_and_tolerates_nulls() {
    assert!(serde_json::from_value::<SessionPropertiesUpdate>(json!({"labels": ["a"]})).is_err());
    assert!(serde_json::from_value::<SessionPropertiesUpdate>(
        json!({"set": [{"name": "a", "value": "b", "extra": 1}]})
    )
    .is_err());
    assert_eq!(
        update(json!({"set": null, "clear": null, "add_labels": null, "remove_labels": null})),
        SessionPropertiesUpdate::default()
    );
}

#[test]
fn metadata_round_trip_drops_an_empty_namespace() {
    let mut metadata = SessionMetadata::new(
        "session-1",
        super::super::SessionInitializer::named("metis", Default::default()),
    );
    assert!(session_properties(&metadata).unwrap().is_empty());

    let mut properties = SessionProperties::default();
    applied(&mut properties, json!({"add_labels": ["bug"]})).unwrap();
    store_properties(&mut metadata, &properties).unwrap();
    assert_eq!(session_properties(&metadata).unwrap(), properties);

    store_properties(&mut metadata, &SessionProperties::default()).unwrap();
    assert!(!metadata
        .extensions
        .contains_key(SESSION_PROPERTIES_NAMESPACE));
}

#[test]
fn http_urls_must_be_written_exactly() {
    for url in [
        "https://harnx.example/agents/pantheon%2Fatlas/sessions/abc123",
        "http://localhost:8000/",
        "https://[::1]:8443/x",
    ] {
        assert!(is_http_url(url), "{url}");
    }
    for url in [
        "harnx.example/agents",
        "https:///agents",
        "https://harnx.example/a b",
        "https://harnx.example\\agents",
        "mailto:someone@example.com",
        "https://token@harnx.example/",
    ] {
        assert!(!is_http_url(url), "{url}");
    }
}

#[test]
fn web_session_url_is_never_inherited() {
    let message = error(json!({"set": [
        {"name": "web_session_url", "value": "https://harnx.example/", "inherit": true}
    ]}));
    assert!(message.contains("never inherited"), "{message}");

    let mut properties = SessionProperties::default();
    applied(
        &mut properties,
        json!({"set": [{"name": "web_session_url", "value": "https://harnx.example/"}]}),
    )
    .unwrap();
    assert!(!properties.get("web_session_url").unwrap().inherit);

    // A record written elsewhere with the flag set still isn't copied.
    let foreign = self::properties(json!({
        "web_session_url": {"value": "https://harnx.example/", "inherit": true},
    }));
    assert!(foreign.inherited().is_empty());
}

#[test]
fn a_supplied_value_replaces_one_harnx_recorded() {
    let mut properties = self::properties(json!({
        "web_session_url": {
            "value": "http://harnx-serve.internal:8000/",
            "inherit": false,
            "source": "inferred",
        },
    }));
    assert_eq!(
        properties.get("web_session_url").unwrap().source,
        Some(PropertySource::Inferred)
    );
    applied(
        &mut properties,
        json!({"set": [{"name": "web_session_url", "value": "https://harnx.example/"}]}),
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(&properties).unwrap(),
        json!({"web_session_url": {"value": "https://harnx.example/", "inherit": false}})
    );
}

#[test]
fn an_unknown_source_reads_as_none() {
    let properties = self::properties(json!({
        "web_session_url": {"value": "https://harnx.example/", "inherit": false, "source": "frontend"},
    }));
    assert_eq!(properties.get("web_session_url").unwrap().source, None);
}

#[test]
fn an_explicit_write_clears_worker_provenance_but_keeps_unknown_attributes() {
    for source in [Some("configured"), None] {
        let mut properties = self::properties(json!({
            "web_session_url": {
                "value": "https://harnx.example/",
                "inherit": false,
                "source": source,
                "recorded_by": "worker",
                "future_attribute": 42,
            },
        }));
        assert!(applied(
            &mut properties,
            json!({"set": [{"name": "web_session_url", "value": "https://harnx.example/"}]}),
        )
        .unwrap());
        assert_eq!(
            serde_json::to_value(&properties).unwrap(),
            json!({"web_session_url": {
                "value": "https://harnx.example/",
                "inherit": false,
                "future_attribute": 42,
            }})
        );
    }
}
