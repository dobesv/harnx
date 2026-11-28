use harnx_core::json_schema::JsonSchema;
use harnx_openapi_tools::{Catalog, SpecDocument};
use harnx_toolset::Registration;
use serde_json::json;

use super::media_contract::{check, validators};

fn jira() -> Catalog {
    let document = SpecDocument::parse(include_bytes!("../fixtures/jira-swagger-v3.json")).unwrap();
    Catalog::build(&document, "jira").unwrap()
}

#[test]
fn jira_repeated_columns_and_attachment_parts_validate_after_core_conversion() {
    harnx_core::require_nextest();
    let catalog = jira();
    let mut args = json!({"body": {
        "content_type": "application/x-www-form-urlencoded", "encoding": "form",
        "fields": [{"name": "columns", "value": "summary"}, {"name": "columns", "value": "status"}]
    }});
    for name in [
        "setIssueNavigatorDefaultColumns",
        "setUserColumns",
        "setColumns",
    ] {
        if name == "setColumns" {
            args["path"] = json!({"id": 42});
        }
        check(
            &validators(catalog.find_operation(name).unwrap()),
            &args,
            true,
        );
    }
    let args = json!({"path": {"issueIdOrKey": "PROJ-7"}, "body": {
        "content_type": "multipart/form-data", "encoding": "multipart", "parts": [
            {"name": "file", "base64": "YQ==", "filename": "a.txt"},
            {"name": "file", "base64": "Yg==", "filename": "b.txt"}
        ]
    }});
    check(
        &validators(catalog.find_operation("addAttachment").unwrap()),
        &args,
        true,
    );
}

#[test]
fn jira_avatar_envelopes_accept_concrete_image_and_require_bytes() {
    harnx_core::require_nextest();
    let catalog = jira();
    let cases = [
        (
            "createIssueTypeAvatar",
            json!({"id": "42"}),
            Some(json!({"size": 1})),
        ),
        ("createProjectAvatar", json!({"projectIdOrKey": "EX"}), None),
        (
            "storeAvatar",
            json!({"type": "project", "entityId": "42"}),
            Some(json!({"size": 1})),
        ),
    ];
    for (name, path, query) in cases {
        let mut args = json!({"path": path, "body": {
            "content_type": "image/png", "encoding": "base64", "base64": "YQ=="
        }});
        if let Some(query) = query {
            args["query"] = query;
        }
        let validate = validators(catalog.find_operation(name).unwrap());
        check(&validate, &args, true);
        args["body"].as_object_mut().unwrap().remove("base64");
        check(&validate, &args, false);
    }
}

fn registration(catalog: &Catalog) -> Registration {
    Registration {
        package: None,
        config: String::new(),
        server: "jira".into(),
        tools: catalog.tool_specs(),
        schema_version: 1,
        proto_version: 1,
        user_id: None,
        ownerless_only: false,
        connection_id: None,
    }
}

fn size(registration: &Registration) -> (usize, usize, usize) {
    let schemas = registration
        .tools
        .iter()
        .map(|tool| serde_json::to_vec(&tool.input_schema).unwrap().len())
        .sum();
    let tools = serde_json::to_vec(&registration.tools).unwrap().len();
    (
        schemas,
        tools,
        serde_json::to_vec(registration).unwrap().len(),
    )
}

#[test]
fn jira_catalog_bytes_and_validator_coverage_through_core_conversion() {
    harnx_core::require_nextest();
    let catalog = jira();
    let mut registration = registration(&catalog);
    assert_eq!(registration.tools.len(), 630);
    assert_eq!(catalog.schema_diagnostics.len(), 3);
    let raw = size(&registration);
    for tool in &mut registration.tools {
        assert!(
            jsonschema::validator_for(&tool.input_schema).is_ok(),
            "{} raw",
            tool.name
        );
        tool.input_schema = JsonSchema::from_tool_schema(std::mem::take(&mut tool.input_schema))
            .unwrap()
            .into_value();
        assert!(
            jsonschema::validator_for(&tool.input_schema).is_ok(),
            "{} core",
            tool.name
        );
    }
    let core = size(&registration);
    println!("Jira bytes (schemas-only, ToolSpecs, Registration): raw={raw:?}, core={core:?}");
    assert!(
        raw.2 < 900_000,
        "raw Registration must leave broker headroom"
    );
}
