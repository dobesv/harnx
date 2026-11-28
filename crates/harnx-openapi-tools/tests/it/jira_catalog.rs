//! Acceptance tests against the pinned official Jira Cloud platform REST v3
//! fixture: inventory equality, naming, size, core conversion, constructible
//! requests, and the reviewed contract's media shapes. No network, no live
//! Jira, no authentication.

use crate::common;

use std::collections::BTreeSet;

use harnx_core::json_schema::JsonSchema;
use harnx_openapi_tools::{BodyInput, Catalog, MediaKind, SpecKind};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const FIXTURE: &str = "jira-swagger-v3.json";
const PINNED_SHA256: &str = "b15c20cebac907b92ac1e812dc93fa087cc267c379c8a23c685c40f3d99b26a7";
const EXPECTED_OPERATIONS: usize = 630;
const EXPECTED_DEPRECATED: usize = 29;
const EXPECTED_NONSTANDARD_PATH_OPS: usize = 18;
const SERVER: &str = "jira";

fn catalog() -> Catalog {
    let document = common::load_fixture(FIXTURE);
    Catalog::build(&document, SERVER).expect("the pinned Jira fixture must normalize")
}

fn raw_fixture_value() -> Value {
    let bytes = std::fs::read(common::fixture_path(FIXTURE)).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// The `(method, path)` pairs the raw document advertises, derived
/// independently of the catalog code under test.
fn expected_inventory() -> BTreeSet<(String, String)> {
    let root = raw_fixture_value();
    let methods = [
        "get", "put", "post", "delete", "options", "head", "patch", "trace",
    ];
    let mut inventory = BTreeSet::new();
    for (path, path_item) in root["paths"].as_object().unwrap() {
        for (key, operation) in path_item.as_object().unwrap() {
            if methods.contains(&key.as_str()) && operation.is_object() {
                inventory.insert((key.clone(), path.clone()));
            }
        }
    }
    inventory
}

#[test]
fn pinned_fixture_hash_matches() {
    harnx_core::require_nextest();
    let bytes = std::fs::read(common::fixture_path(FIXTURE)).unwrap();
    assert_eq!(bytes.len(), 2_515_383);
    let digest = Sha256::digest(&bytes);
    let hash: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(hash, PINNED_SHA256, "the pinned fixture changed on disk");
}

#[test]
fn inventory_is_complete_including_deprecated_and_nonstandard_paths() {
    harnx_core::require_nextest();
    let catalog = catalog();
    assert_eq!(catalog.spec_kind, SpecKind::OpenApi3);
    assert_eq!(catalog.spec_version, "3.0.1");
    let actual: BTreeSet<(String, String)> = catalog.inventory().into_iter().collect();
    assert_eq!(
        actual.len(),
        EXPECTED_OPERATIONS,
        "the pinned fixture must expose every operation"
    );
    let expected = expected_inventory();
    assert_eq!(
        actual, expected,
        "catalog inventory must equal the document"
    );

    let deprecated = catalog
        .operations
        .iter()
        .filter(|operation| operation.deprecated)
        .count();
    assert_eq!(deprecated, EXPECTED_DEPRECATED);
    let nonstandard = catalog
        .operations
        .iter()
        .filter(|operation| !operation.path.starts_with("/rest/api/3"))
        .count();
    assert_eq!(nonstandard, EXPECTED_NONSTANDARD_PATH_OPS);
    // Deprecated operations stay callable, with the marker in the description.
    let deprecated_op = catalog
        .operations
        .iter()
        .find(|operation| operation.deprecated)
        .expect("deprecated operations present");
    assert!(deprecated_op.description.ends_with("(deprecated)"));
}

#[test]
fn names_are_unique_valid_and_within_the_server_prefix_budget() {
    harnx_core::require_nextest();
    let catalog = catalog();
    let prefix_budget = harnx_openapi_tools::MAX_MODEL_TOOL_NAME_LEN - (SERVER.len() + 1);
    let mut seen = BTreeSet::new();
    for operation in &catalog.operations {
        let name = &operation.raw_tool_name;
        assert!(
            name.len() <= prefix_budget,
            "raw name {name:?} exceeds the {prefix_budget}-character budget"
        );
        assert!(
            !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "raw name {name:?} is not a valid tool name"
        );
        assert!(!name.starts_with(|c: char| c.is_ascii_digit()), "{name:?}");
        assert!(seen.insert(name.clone()), "duplicate raw name {name:?}");
        // Stable mapping back to (method, path).
        assert_eq!(
            catalog
                .find_operation(name)
                .map(|found| found.method.clone()),
            Some(operation.method.clone())
        );
        assert_eq!(
            catalog
                .find_by_method_path(&operation.method, &operation.path)
                .map(|found| found.raw_tool_name.clone()),
            Some(name.clone())
        );
        // Every unqualified model-facing name fits the 64-character limit.
        let model_name = format!("{SERVER}_{name}");
        assert!(model_name.len() <= harnx_openapi_tools::MAX_MODEL_TOOL_NAME_LEN);
    }
    // A known operation ID passes through unchanged.
    assert!(catalog.find_operation("createIssue").is_some());
    assert!(catalog.find_operation("addAttachment").is_some());
}

#[test]
fn every_generated_schema_passes_actual_core_conversion() {
    harnx_core::require_nextest();
    let catalog = catalog();
    let mut recursive_operations = 0;
    for spec in catalog.tool_specs() {
        let converted = JsonSchema::from_tool_schema(spec.input_schema)
            .unwrap_or_else(|error| panic!("core conversion failed for {}: {error}", spec.name));
        // Recursive references survive: definitions are retained when a $ref
        // cycle reaches back into them.
        if converted.as_value().get("$defs").is_some() {
            recursive_operations += 1;
        }
    }
    assert!(
        recursive_operations >= 5,
        "expected at least five operations with recursive request schemas, found \
         {recursive_operations}"
    );
}

#[test]
fn every_operation_is_constructible_except_the_pinned_contradictions() {
    harnx_core::require_nextest();
    let catalog = catalog();
    let mut unsatisfiable: Vec<String> = Vec::new();
    for operation in &catalog.operations {
        let schema = &operation.input_schema;
        let defs = schema
            .get("$defs")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let arguments = common::synthesize(schema, &defs);
        let validator = jsonschema::validator_for(schema).unwrap_or_else(|error| {
            panic!("{}: schema does not compile: {error}", operation.context())
        });
        if validator.validate(&arguments).is_err() {
            unsatisfiable.push(operation.context());
        }
    }
    // The pinned fixture ships two self-contradictory schemas (a `required`
    // name with no property under `additionalProperties: false`). They stay
    // registered with their schemas kept verbatim and a precise diagnostic —
    // no compatibility weakening — so exactly these two operations cannot
    // construct a valid request. The all-constructible acceptance stays
    // blocked pending a planning decision on the vendor typos; this test
    // pins the blocked set instead of skipping it.
    unsatisfiable.sort();
    assert_eq!(
        unsatisfiable,
        vec![
            "POST /rest/api/3/bulk/issues/move".to_string(),
            "PUT /rest/api/3/issuesecurityschemes/{schemeId}/level/{levelId}/member".to_string(),
        ],
        "only the two pinned contradictory operations may be unsatisfiable: {unsatisfiable:?}"
    );
    for (needle, name) in [
        (
            "targetToMultipleSourceMapping",
            "POST /rest/api/3/bulk/issues/move",
        ),
        (
            "securitySchemeLevelMembers",
            "PUT /rest/api/3/issuesecurityschemes/{schemeId}/level/{levelId}/member",
        ),
    ] {
        assert!(
            catalog
                .schema_diagnostics
                .iter()
                .any(|message| message.contains(name) && message.contains(needle)),
            "expected a contradiction diagnostic for {name} mentioning {needle}: {:?}",
            catalog.schema_diagnostics
        );
    }
    // Both operations stay registered: impossible schemas are diagnostics,
    // never dropped from the inventory.
    assert!(catalog
        .find_by_method_path("post", "/rest/api/3/bulk/issues/move")
        .is_some());
}

#[test]
fn serialized_catalog_fits_a_stock_megabyte_registration_with_headroom() {
    harnx_core::require_nextest();
    let catalog = catalog();
    let bytes = catalog.serialized_tool_catalog_bytes();
    assert!(
        bytes < 900_000,
        "compact catalog serialized to {bytes} bytes, above the 900 KB ceiling \
         (stock broker max_payload is 1 MiB)"
    );
    // Every operation contributes a spec, so the payload is per-operation
    // compact rather than accidentally small by dropping tools.
    assert_eq!(catalog.tool_specs().len(), EXPECTED_OPERATIONS);
}

#[test]
fn json_and_yaml_documents_produce_identical_catalogs() {
    harnx_core::require_nextest();
    let json_document = common::load_fixture(FIXTURE);
    let yaml_text = serde_yaml::to_string(&json_document.root).unwrap();
    let yaml_document = harnx_openapi_tools::SpecDocument::from_yaml(yaml_text.as_bytes())
        .expect("the YAML rendering must parse");
    let json_catalog = Catalog::build(&json_document, SERVER).unwrap();
    let yaml_catalog = Catalog::build(&yaml_document, SERVER).unwrap();
    assert_eq!(json_catalog.inventory(), yaml_catalog.inventory());
    assert_eq!(
        serde_json::to_string(&json_catalog.tool_specs()).unwrap(),
        serde_json::to_string(&yaml_catalog.tool_specs()).unwrap(),
        "JSON and YAML documents must normalize identically"
    );
}

fn envelope_of(catalog: &Catalog, raw_name: &str) -> Value {
    let operation = catalog.find_operation(raw_name).unwrap();
    let body = operation.body.as_ref().unwrap();
    assert!(
        matches!(body.input, BodyInput::Envelope(ref schema) if schema.is_object()),
        "{raw_name} must publish a body envelope"
    );
    match &body.input {
        BodyInput::Envelope(schema) => schema.clone(),
        BodyInput::Direct(_) => unreachable!(),
    }
}

#[test]
fn add_attachment_publishes_generic_multipart_parts() {
    harnx_core::require_nextest();
    let catalog = catalog();
    let envelope = envelope_of(&catalog, "addAttachment");
    // Single multipart entry: concrete content type, multipart encoding,
    // generic named parts. The Java MultipartFile object schema must not be
    // forced onto the wire representation.
    assert_eq!(
        envelope["properties"]["content_type"]["enum"],
        json!(["multipart/form-data"])
    );
    assert_eq!(
        envelope["properties"]["encoding"]["enum"],
        json!(["multipart"])
    );
    assert!(envelope["properties"]["parts"]["items"]["properties"]["base64"].is_object());
    // No source property names to constrain: the part name is free-form.
    assert_eq!(
        envelope["properties"]["parts"]["items"]["properties"]["name"],
        json!({"type": "string"})
    );
    let operation = catalog.find_operation("addAttachment").unwrap();
    let body = operation.body.as_ref().unwrap();
    assert_eq!(body.branches[0].kind, MediaKind::Multipart);
    assert_eq!(body.branches[0].media_type, "multipart/form-data");
}

#[test]
fn plans_json_patch_operations_send_arrays_despite_object_schemas() {
    harnx_core::require_nextest();
    let catalog = catalog();
    for raw_name in ["updatePlan", "updateAtlassianTeam", "updatePlanOnlyTeam"] {
        let operation = catalog
            .find_operation(raw_name)
            .unwrap_or_else(|| panic!("{raw_name} must be registered"));
        let body = operation.body.as_ref().unwrap();
        assert!(
            matches!(&body.input, BodyInput::Direct(schema) if schema["type"] == json!("array")),
            "{raw_name} must publish a direct JSON Patch array, not the documented object schema"
        );
        assert_eq!(body.branches[0].media_type, "application/json-patch+json");
        assert_eq!(body.branches[0].kind, MediaKind::JsonPatch);
    }
}

#[test]
fn column_operations_support_repeated_fields_through_their_wildcard_entry() {
    harnx_core::require_nextest();
    let catalog = catalog();
    for raw_name in [
        "setIssueNavigatorDefaultColumns",
        "setUserColumns",
        "setColumns",
    ] {
        let envelope = envelope_of(&catalog, raw_name);
        let form = common::media_variant(&envelope, "form", "application/x-www-form-urlencoded");
        assert_eq!(
            form["properties"]["fields"]["items"]["properties"]["name"]["enum"],
            json!(["columns"])
        );
        assert_eq!(
            form["properties"]["fields"]["items"]["required"],
            json!(["name", "value"])
        );
        // The wildcard entry keeps the URL-encoded fields encoder available.
        let operation = catalog.find_operation(raw_name).unwrap();
        let body = operation.body.as_ref().unwrap();
        assert!(
            body.branches
                .iter()
                .any(|branch| branch.kind == MediaKind::Wildcard),
            "{raw_name} must keep its wildcard branch for dispatch"
        );
    }
}

#[test]
fn wildcard_avatar_uploads_publish_generic_binary_and_json_encoders() {
    harnx_core::require_nextest();
    let catalog = catalog();
    for raw_name in [
        "createIssueTypeAvatar",
        "createProjectAvatar",
        "storeAvatar",
    ] {
        let envelope = envelope_of(&catalog, raw_name);
        let binary = common::media_variant(&envelope, "base64", "image/png");
        assert_eq!(binary["properties"]["base64"]["type"], "string");
        let json = common::media_variant(&envelope, "json", "application/json");
        assert!(json["properties"]["value"].is_object());
    }
}

#[test]
fn text_and_json_preference_operation_offers_both_encodings() {
    harnx_core::require_nextest();
    let catalog = catalog();
    let envelope = envelope_of(&catalog, "setPreference");
    let json = common::media_variant(&envelope, "json", "application/json");
    assert_eq!(json["properties"]["value"], json!({"type": "string"}));
    let text = common::media_variant(&envelope, "text", "text/plain");
    assert_eq!(text["properties"]["text"], json!({"type": "string"}));
}

#[test]
fn exact_media_entries_take_precedence_over_wildcards() {
    harnx_core::require_nextest();
    let catalog = catalog();
    let operation = catalog.find_operation("setColumns").unwrap();
    let branches = &operation.body.as_ref().unwrap().branches;
    let selected = harnx_openapi_tools::select_media_branch(branches, "application/json")
        .expect("an exact entry exists");
    assert_eq!(selected.media_type, "application/json");
    let selected = harnx_openapi_tools::select_media_branch(branches, "image/png")
        .expect("the wildcard entry accepts concrete media selection");
    assert_eq!(selected.media_type, "*/*");
    assert!(harnx_openapi_tools::select_media_branch(branches, "*/*").is_none());
}
