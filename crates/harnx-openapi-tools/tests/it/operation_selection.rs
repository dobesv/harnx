//! Acceptance tests for the optional `--include-operation` selection layer:
//! the default stays the full pinned catalog, selectors form a positive
//! union, and selection governs declarations and dispatch together. No
//! network, no auth.

use crate::common;

use harnx_core::json_schema::JsonSchema;
use harnx_openapi_tools::{
    build_catalog_with_controls, select_operations, Catalog, OperationSelection, SpecDocument,
    SpecPatchLimits,
};
use serde_json::{json, Value};

const FIXTURE: &str = "jira-swagger-v3.json";
const SERVER: &str = "jira";
const EXPECTED_OPERATIONS: usize = 630;

fn selectors(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| value.to_string()).collect()
}

fn jira_document() -> SpecDocument {
    common::load_fixture(FIXTURE)
}

fn inline_document(value: Value) -> SpecDocument {
    SpecDocument::from_value(value).unwrap()
}

#[test]
fn no_selectors_keeps_the_full_pinned_catalog() {
    harnx_core::require_nextest();
    let document = jira_document();
    let plain = Catalog::build(&document, SERVER).unwrap();
    let controlled =
        build_catalog_with_controls(&document, SERVER, &[], SpecPatchLimits::default(), &[])
            .unwrap();
    assert_eq!(controlled.operations.len(), EXPECTED_OPERATIONS);
    assert_eq!(
        serde_json::to_string(&plain.tool_specs()).unwrap(),
        serde_json::to_string(&controlled.tool_specs()).unwrap(),
        "empty patches and empty selectors must reproduce Catalog::build"
    );
    // The identity also holds on an existing catalog.
    let identity =
        select_operations(&plain, &document, &[]).expect("empty selector list is the identity");
    assert_eq!(identity.operations.len(), EXPECTED_OPERATIONS);
    assert_eq!(
        serde_json::to_string(&identity.tool_specs()).unwrap(),
        serde_json::to_string(&plain.tool_specs()).unwrap()
    );
    // Pre-validation accepts an empty list.
    OperationSelection::parse(&[]).expect("empty selector list parses");
}

#[test]
fn id_selectors_match_source_operation_ids_case_sensitively() {
    harnx_core::require_nextest();
    let document = jira_document();
    let catalog = Catalog::build(&document, SERVER).unwrap();
    let selected = select_operations(&catalog, &document, &selectors(&["id:createIssue"])).unwrap();
    assert_eq!(
        selected.inventory(),
        vec![("post".to_string(), "/rest/api/3/issue".to_string())]
    );
    assert!(selected.find_operation("createIssue").is_some());

    // A glob over the source IDs, pinned against the fixture inventory.
    let selected = select_operations(&catalog, &document, &selectors(&["id:create*"])).unwrap();
    assert_eq!(selected.operations.len(), 44);
    assert!(
        selected
            .operations
            .iter()
            .all(|operation| operation.raw_tool_name.starts_with("create")),
        "every id:create* selection must come from a create* operationId"
    );

    // IDs are case-sensitive: no match is an error, not an empty catalog.
    let error = select_operations(&catalog, &document, &selectors(&["id:createissue"]))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("operation selector 1 of 1 matched no operation"),
        "{error}"
    );
    assert!(error.contains("id:createissue"), "{error}");
}

#[test]
fn route_selectors_match_method_and_path_templates() {
    harnx_core::require_nextest();
    let document = jira_document();
    let catalog = Catalog::build(&document, SERVER).unwrap();

    // Method ASCII case-insensitive, path exact.
    for selector in [
        "route:POST /rest/api/3/issue",
        "route:post /rest/api/3/issue",
    ] {
        let selected = select_operations(&catalog, &document, &selectors(&[selector])).unwrap();
        assert_eq!(
            selected.inventory(),
            vec![("post".to_string(), "/rest/api/3/issue".to_string())],
            "{selector}"
        );
    }

    // `*` method, and path globs spanning slashes.
    let selected = select_operations(
        &catalog,
        &document,
        &selectors(&["route:* /rest/api/3/issue/*"]),
    )
    .unwrap();
    assert_eq!(selected.operations.len(), 59);
    let selected = select_operations(
        &catalog,
        &document,
        &selectors(&["route:GET /rest/api/3/issue/*"]),
    )
    .unwrap();
    assert_eq!(selected.operations.len(), 22);
    let selected = select_operations(
        &catalog,
        &document,
        &selectors(&["route:POST /rest/api/3/bulk/*"]),
    )
    .unwrap();
    assert_eq!(selected.operations.len(), 6);

    // The path template is matched case-sensitively.
    let error = select_operations(
        &catalog,
        &document,
        &selectors(&["route:GET /REST/API/3/ISSUE"]),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("matched no operation"), "{error}");
}

#[test]
fn union_is_positive_deduplicated_and_document_ordered() {
    harnx_core::require_nextest();
    let document = jira_document();
    let catalog = Catalog::build(&document, SERVER).unwrap();
    // Repeats and overlapping selectors union to the same set, never to
    // duplicates, and the result keeps document order rather than
    // selector order (createIssue precedes addAttachment in the fixture).
    let selected = select_operations(
        &catalog,
        &document,
        &selectors(&[
            "id:addAttachment",
            "id:createIssue",
            "route:POST /rest/api/3/issue",
            "id:createIssue",
        ]),
    )
    .unwrap();
    assert_eq!(
        selected.inventory(),
        vec![
            ("post".to_string(), "/rest/api/3/issue".to_string()),
            (
                "post".to_string(),
                "/rest/api/3/issue/{issueIdOrKey}/attachments".to_string()
            ),
        ],
        "union must dedupe and keep document order"
    );
    let specs = selected.tool_specs();
    let names: Vec<&str> = specs.iter().map(|spec| spec.name.as_str()).collect();
    assert_eq!(names, vec!["createIssue", "addAttachment"]);
}

#[test]
fn individually_unmatched_and_invalid_selectors_error() {
    harnx_core::require_nextest();
    let document = jira_document();
    let catalog = Catalog::build(&document, SERVER).unwrap();

    let error = select_operations(&catalog, &document, &selectors(&["id:nope"]))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("operation selector 1 of 1 matched no operation"),
        "{error}"
    );
    // A bad selector fails even when other selectors match.
    let error = select_operations(
        &catalog,
        &document,
        &selectors(&["id:createIssue", "id:nope"]),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("operation selector 2 of 2 matched no operation"),
        "{error}"
    );
    // Invalid syntax is rejected before matching, with the value named.
    let error = select_operations(&catalog, &document, &selectors(&["bogus"]))
        .unwrap_err()
        .to_string();
    assert!(error.contains("invalid operation selector"), "{error}");
    assert!(error.contains("bogus"), "{error}");
    // No regexp/brace/comma/negation syntax exists: those bytes are literal
    // and so match nothing here.
    for expression in [
        "id:createIssue|addAttachment",
        "id:{createIssue,addAttachment}",
        "id:createIssu[eE]",
        "id:!createIssue",
        "id:createIssue,addAttachment",
    ] {
        let error = select_operations(&catalog, &document, &selectors(&[expression]))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("matched no operation"),
            "{expression} must not act as syntax: {error}"
        );
    }
}

/// A small document whose two operations share one `operationId`, plus a
/// third distinct operation.
fn duplicate_id_document() -> SpecDocument {
    inline_document(json!({
        "swagger": "2.0",
        "paths": {
            "/a": {"get": {"operationId": "dupOp"}},
            "/b": {"post": {"operationId": "dupOp"}},
            "/c": {"get": {"operationId": "otherOp"}}
        }
    }))
}

#[test]
fn duplicate_ids_select_all_with_stable_collision_names() {
    harnx_core::require_nextest();
    let document = duplicate_id_document();
    let full = Catalog::build(&document, "srv").unwrap();
    assert_eq!(full.operations.len(), 3);
    let full_names: Vec<String> = full
        .operations
        .iter()
        .map(|operation| operation.raw_tool_name.clone())
        .collect();
    assert_ne!(full_names[0], full_names[1], "the collision must be broken");
    assert_eq!(full_names[2], "otherOp");

    // An id selector selects EVERY operation carrying that ID.
    let selected = select_operations(&full, &document, &selectors(&["id:dupOp"])).unwrap();
    assert_eq!(selected.inventory().len(), 2);
    let selected_names: Vec<String> = selected
        .operations
        .iter()
        .map(|operation| operation.raw_tool_name.clone())
        .collect();
    assert_eq!(
        selected_names,
        vec![full_names[0].clone(), full_names[1].clone()]
    );

    // Filtering away a collision participant leaves the survivor with its
    // full-inventory name, including any collision suffix.
    for (route_selector, name) in [
        ("route:GET /a", &full_names[0]),
        ("route:POST /b", &full_names[1]),
    ] {
        let selected = select_operations(&full, &document, &selectors(&[route_selector])).unwrap();
        assert_eq!(selected.operations.len(), 1, "{route_selector}");
        assert_eq!(
            &selected.operations[0].raw_tool_name, name,
            "{route_selector}"
        );
    }
}

#[test]
fn route_fallback_selects_operations_without_ids() {
    harnx_core::require_nextest();
    let document = inline_document(json!({
        "swagger": "2.0",
        "paths": {
            "/x": {"get": {}},
            "/y": {"post": {"operationId": "knownOp"}}
        }
    }));
    let full = Catalog::build(&document, "srv").unwrap();
    assert_eq!(full.operations.len(), 2);

    // An operation without an `operationId` is only reachable by route.
    let selected = select_operations(&full, &document, &selectors(&["route:GET /x"])).unwrap();
    assert_eq!(
        selected.inventory(),
        vec![("get".to_string(), "/x".to_string())]
    );
    // `id:*` never matches it, and matching nothing would be an error —
    // the union with the other operation still keeps the id-less one out.
    let selected =
        select_operations(&full, &document, &selectors(&["id:*", "route:GET /x"])).unwrap();
    assert_eq!(
        selected.operations.len(),
        2,
        "both are selected by the union"
    );
    let selected = select_operations(&full, &document, &selectors(&["id:knownOp"])).unwrap();
    assert_eq!(selected.operations.len(), 1);
    assert!(selected.find_by_method_path("get", "/x").is_none());
}

#[test]
fn selection_governs_declarations_and_dispatch_together() {
    harnx_core::require_nextest();
    let document = jira_document();
    let catalog = Catalog::build(&document, SERVER).unwrap();
    let selected = select_operations(
        &catalog,
        &document,
        &selectors(&["id:createIssue", "id:addAttachment"]),
    )
    .unwrap();
    // Discovery: exactly the selected declarations.
    let specs = selected.tool_specs();
    let names: Vec<&str> = specs.iter().map(|spec| spec.name.as_str()).collect();
    assert_eq!(names.len(), 2);
    assert!(names.contains(&"createIssue"));
    assert!(names.contains(&"addAttachment"));
    // Dispatch: selected lookups resolve, excluded ones do not.
    assert!(selected.find_operation("createIssue").is_some());
    assert!(selected
        .find_by_method_path("post", "/rest/api/3/issue")
        .is_some());
    assert!(selected.find_operation("addAttachment").is_some());
    // The same path is also served by GET, which is excluded: neither the
    // excluded named lookup nor the excluded method/path lookup resolves.
    assert!(selected
        .find_by_method_path("get", "/rest/api/3/issue")
        .is_none());
    assert!(selected.find_operation("getMyPermissions").is_none());
    assert!(catalog.find_operation("getMyPermissions").is_some());
    // Excluded declarations are gone from the published size too.
    assert!(selected.serialized_tool_catalog_bytes() < catalog.serialized_tool_catalog_bytes());
}

#[test]
fn selection_uses_the_effective_patched_source() {
    harnx_core::require_nextest();
    let document = jira_document();
    let patches =
        selectors(&[r#".paths["/rest/api/3/issue"].post.operationId = "renamedCreateIssue""#]);
    // The patched ID is selectable; the pre-patch ID no longer exists.
    let selected = build_catalog_with_controls(
        &document,
        SERVER,
        &patches,
        SpecPatchLimits::default(),
        &selectors(&["id:renamedCreateIssue"]),
    )
    .unwrap();
    assert_eq!(
        selected.inventory(),
        vec![("post".to_string(), "/rest/api/3/issue".to_string())]
    );
    let error = build_catalog_with_controls(
        &document,
        SERVER,
        &patches,
        SpecPatchLimits::default(),
        &selectors(&["id:createIssue"]),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("matched no operation"), "{error}");

    // A patch can add the ID that a selector needs, and a document-order
    // pair of (patch, selector) runs in that order on inline sources too.
    let document = inline_document(json!({
        "swagger": "2.0",
        "paths": {"/z": {"get": {}}}
    }));
    let patches = selectors(&[r#".paths["/z"].get.operationId = "zOp""#]);
    let selected = build_catalog_with_controls(
        &document,
        "srv",
        &patches,
        SpecPatchLimits::default(),
        &selectors(&["id:zOp"]),
    )
    .unwrap();
    assert_eq!(
        selected.inventory(),
        vec![("get".to_string(), "/z".to_string())]
    );
    let error = build_catalog_with_controls(
        &document,
        "srv",
        &[],
        SpecPatchLimits::default(),
        &selectors(&["id:zOp"]),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("matched no operation"), "{error}");
}

#[test]
fn unsupported_constructs_cannot_be_hidden_by_selection() {
    harnx_core::require_nextest();
    // /good is the only selected operation; /bad is genuinely unselected
    // and carries an unsupported cookie parameter plus a broken `$ref`.
    // Whole-document normalization runs before selection, so /bad must
    // fail the build — it cannot be silently dropped by the selector.
    let document = inline_document(json!({
        "openapi": "3.0.1",
        "paths": {
            "/good": {
                "get": {
                    "operationId": "goodOp",
                    "responses": {"200": {"description": "ok"}}
                }
            },
            "/bad": {
                "get": {
                    "operationId": "badOp",
                    "parameters": [
                        {"name": "session", "in": "cookie", "required": true,
                         "schema": {"type": "string"}}
                    ],
                    "requestBody": {
                        "content": {"application/json": {
                            "schema": {"$ref": "#/components/schemas/Missing"}
                        }}
                    },
                    "responses": {"200": {"description": "ok"}}
                }
            }
        }
    }));
    let error = build_catalog_with_controls(
        &document,
        "srv",
        &[],
        SpecPatchLimits::default(),
        &selectors(&["id:goodOp"]),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("/bad"),
        "normalization must reject the unselected /bad operation before selection: {error}"
    );
    assert!(
        !error.contains("matched no operation"),
        "the failure must not be about selection: {error}"
    );

    // Control: the same selection on the same document without /bad
    // succeeds, so the failure above is /bad's, not the selector's.
    let good_only = inline_document(json!({
        "openapi": "3.0.1",
        "paths": {
            "/good": {
                "get": {
                    "operationId": "goodOp",
                    "responses": {"200": {"description": "ok"}}
                }
            }
        }
    }));
    let selected = build_catalog_with_controls(
        &good_only,
        "srv",
        &[],
        SpecPatchLimits::default(),
        &selectors(&["id:goodOp"]),
    )
    .unwrap();
    assert_eq!(selected.operations.len(), 1);
}

#[test]
fn selected_catalogs_retain_raw_and_core_schemas() {
    harnx_core::require_nextest();
    let document = jira_document();
    let catalog = Catalog::build(&document, SERVER).unwrap();
    let selected = select_operations(
        &catalog,
        &document,
        &selectors(&["id:create*", "route:GET /rest/api/3/issue/*"]),
    )
    .unwrap();
    assert!(selected.operations.len() < EXPECTED_OPERATIONS && !selected.operations.is_empty());
    // Every selected declaration is a real raw spec and survives the actual
    // core conversion unchanged by selection.
    let full_specs: Vec<String> = catalog
        .tool_specs()
        .iter()
        .map(|spec| {
            serde_json::to_string(&serde_json::json!({
                "name": spec.name,
                "input_schema": spec.input_schema,
            }))
            .unwrap()
        })
        .collect();
    for spec in selected.tool_specs() {
        assert!(
            full_specs.contains(
                &serde_json::to_string(&serde_json::json!({
                    "name": spec.name,
                    "input_schema": spec.input_schema,
                }))
                .unwrap()
            ),
            "selected spec {} must be a verbatim full-catalog spec",
            spec.name
        );
        JsonSchema::from_tool_schema(spec.input_schema.clone())
            .unwrap_or_else(|error| panic!("core conversion failed for {}: {error}", spec.name));
    }
}
