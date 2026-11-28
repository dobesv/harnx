//! Acceptance tests for the ordered `--spec-patch` source-patch support:
//! the unpatched baseline stays blocked exactly as pinned, the explicit
//! witness patches fix exactly the three vendor `required` typos, and the
//! patched catalog is fully constructible. No network, no auth.

use crate::common;

use harnx_core::json_schema::JsonSchema;
use harnx_openapi_tools::{
    apply_spec_patches, validate_spec_patches, Catalog, SpecDocument, SpecPatchLimits,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

const FIXTURE: &str = "jira-swagger-v3.json";
const SERVER: &str = "jira";
const EXPECTED_OPERATIONS: usize = 630;
const PINNED_SHA256: &str = "b15c20cebac907b92ac1e812dc93fa087cc267c379c8a23c685c40f3d99b26a7";

/// The explicit, user-authorized witness patches for the pinned vendor
/// typos: exactly three `required` strings across two operations, corrected
/// in occurrence order.
fn witness_patches() -> Vec<String> {
    [
        r#".components.schemas.IssueBulkMovePayload.required[0] = "targetToSourcesMapping""#,
        r#".components.schemas.targetToSourcesMapping.required[4] = "issueIdsOrKeys""#,
        r#".components.schemas.SecuritySchemeMembersRequest.required[0] = "members""#,
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

fn witness_document() -> SpecDocument {
    let document = common::load_fixture(FIXTURE);
    apply_spec_patches(&document, &witness_patches(), SpecPatchLimits::default())
        .expect("the witness patches must apply to the pinned fixture")
}

/// The JSON pointers at which two value trees differ, so a test can pin
/// exactly which source fields a patch list touched.
fn differing_pointers(left: &Value, right: &Value) -> Vec<String> {
    fn walk(left: &Value, right: &Value, prefix: &str, out: &mut Vec<String>) {
        if left == right {
            return;
        }
        match (left, right) {
            (Value::Object(left), Value::Object(right)) => {
                for (key, value) in left {
                    let pointer = format!("{prefix}/{key}");
                    match right.get(key) {
                        Some(other) => walk(value, other, &pointer, out),
                        None => out.push(pointer),
                    }
                }
                for key in right.keys().filter(|key| !left.contains_key(key.as_str())) {
                    out.push(format!("{prefix}/{key}"));
                }
            }
            (Value::Array(left), Value::Array(right)) if left.len() == right.len() => {
                for (index, (value, other)) in left.iter().zip(right).enumerate() {
                    walk(value, other, &format!("{prefix}/{index}"), out);
                }
            }
            _ => out.push(prefix.to_string()),
        }
    }
    let mut pointers = Vec::new();
    walk(left, right, "", &mut pointers);
    pointers.sort();
    pointers
}

/// Operations whose published input schema cannot validate any synthesized
/// request, derived the same way as the pinned baseline test.
fn unsatisfiable_operations(catalog: &Catalog) -> Vec<String> {
    let mut unsatisfiable = Vec::new();
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
    unsatisfiable
}

#[test]
fn unpatched_baseline_stays_blocked_exactly_as_pinned() {
    harnx_core::require_nextest();
    // The fixture bytes and hash are untouched by this feature.
    let bytes = std::fs::read(common::fixture_path(FIXTURE)).unwrap();
    assert_eq!(bytes.len(), 2_515_383);
    let digest = Sha256::digest(&bytes);
    let hash: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(hash, PINNED_SHA256, "the pinned fixture changed on disk");

    let catalog = Catalog::build(&common::load_fixture(FIXTURE), SERVER).unwrap();
    assert_eq!(catalog.operations.len(), EXPECTED_OPERATIONS);
    // Three contradictory-schema diagnostics — one per vendor typo — with no
    // default repair and no weakening.
    assert_eq!(
        catalog.schema_diagnostics.len(),
        3,
        "expected exactly three schema diagnostics, got: {:?}",
        catalog.schema_diagnostics
    );
    for needle in [
        "targetToMultipleSourceMapping",
        "issueIdOrKeys",
        "securitySchemeLevelMembers",
    ] {
        assert!(
            catalog
                .schema_diagnostics
                .iter()
                .any(|message| message.contains(needle)),
            "missing diagnostic for {needle}: {:?}",
            catalog.schema_diagnostics
        );
    }
    let unsatisfiable = unsatisfiable_operations(&catalog);
    assert_eq!(
        unsatisfiable,
        vec![
            "POST /rest/api/3/bulk/issues/move".to_string(),
            "PUT /rest/api/3/issuesecurityschemes/{schemeId}/level/{levelId}/member".to_string(),
        ],
        "exactly the two pinned contradictory operations may be unsatisfiable"
    );
}

#[test]
fn no_patches_is_identity_for_source_and_catalog() {
    harnx_core::require_nextest();
    let document = common::load_fixture(FIXTURE);
    let patched = apply_spec_patches(&document, &[], SpecPatchLimits::default()).expect("identity");
    assert_eq!(
        patched.root, document.root,
        "no patches must not touch the source"
    );
    let plain = Catalog::build(&document, SERVER).unwrap();
    let via_patched =
        Catalog::build_patched(&document, SERVER, &[], SpecPatchLimits::default()).unwrap();
    assert_eq!(
        serde_json::to_string(&plain.tool_specs()).unwrap(),
        serde_json::to_string(&via_patched.tool_specs()).unwrap()
    );
    assert!(validate_spec_patches(&[], SpecPatchLimits::default()).is_ok());
}

#[test]
fn witness_patches_change_exactly_the_three_typos() {
    harnx_core::require_nextest();
    let document = common::load_fixture(FIXTURE);
    let patched = witness_document();
    assert_eq!(
        differing_pointers(&document.root, &patched.root),
        vec![
            "/components/schemas/IssueBulkMovePayload/required/0".to_string(),
            "/components/schemas/SecuritySchemeMembersRequest/required/0".to_string(),
            "/components/schemas/targetToSourcesMapping/required/4".to_string(),
        ],
        "the witness patches must change exactly the three typo'd required strings"
    );
    // The three fixes land verbatim, and the vendor typo strings are gone
    // from the effective source.
    assert_eq!(
        patched.root["components"]["schemas"]["IssueBulkMovePayload"]["required"],
        serde_json::json!(["targetToSourcesMapping"])
    );
    assert_eq!(
        patched.root["components"]["schemas"]["targetToSourcesMapping"]["required"][4],
        serde_json::json!("issueIdsOrKeys")
    );
    assert_eq!(
        patched.root["components"]["schemas"]["SecuritySchemeMembersRequest"]["required"],
        serde_json::json!(["members"])
    );
    let serialized = serde_json::to_string(&patched.root).unwrap();
    for typo in [
        "targetToMultipleSourceMapping",
        "securitySchemeLevelMembers",
    ] {
        assert!(
            !serialized.contains(typo),
            "{typo} must be gone from the patched source"
        );
    }
    // No additionalProperties relaxation anywhere: the false flags survive.
    assert_eq!(
        patched.root["components"]["schemas"]["IssueBulkMovePayload"]["additionalProperties"],
        serde_json::json!(false)
    );
    assert_eq!(
        patched.root["components"]["schemas"]["targetToSourcesMapping"]["additionalProperties"],
        serde_json::json!(false)
    );
    assert_eq!(
        patched.root["components"]["schemas"]["SecuritySchemeMembersRequest"]
            ["additionalProperties"],
        serde_json::json!(false)
    );
    // The compile-only preflight accepts the witness list.
    validate_spec_patches(&witness_patches(), SpecPatchLimits::default())
        .expect("the witness patches must compile");
}

#[test]
fn patched_catalog_is_fully_constructible_and_core_convertible() {
    harnx_core::require_nextest();
    let catalog = Catalog::build_patched(
        &common::load_fixture(FIXTURE),
        SERVER,
        &witness_patches(),
        SpecPatchLimits::default(),
    )
    .expect("the patched fixture must normalize");
    assert_eq!(catalog.operations.len(), EXPECTED_OPERATIONS);
    assert!(
        catalog.schema_diagnostics.is_empty(),
        "the witness patches must clear every contradiction diagnostic: {:?}",
        catalog.schema_diagnostics
    );

    // Every operation constructs a valid request through its published
    // input schema — the acceptance blocked on the vendor typos, unblocked by
    // explicit patches only.
    let unsatisfiable = unsatisfiable_operations(&catalog);
    assert!(
        unsatisfiable.is_empty(),
        "every patched operation must be constructible: {unsatisfiable:?}"
    );

    // Every generated schema also passes the real core conversion (630/630
    // valid), the same gate the unpatched catalog must pass.
    let specs = catalog.tool_specs();
    assert_eq!(specs.len(), EXPECTED_OPERATIONS);
    for spec in &specs {
        JsonSchema::from_tool_schema(spec.input_schema.clone())
            .unwrap_or_else(|error| panic!("core conversion failed for {}: {error}", spec.name));
    }

    // The corrected fields are visible in the published schemas, with the
    // bulk mapping nonempty and additionalProperties:false retained.
    let bulk_move = catalog
        .find_by_method_path("post", "/rest/api/3/bulk/issues/move")
        .expect("bulk move stays registered");
    let defs = bulk_move.input_schema["$defs"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    let payload = defs
        .get("IssueBulkMovePayload")
        .expect("the payload definition is retained");
    assert_eq!(
        payload["required"],
        serde_json::json!(["targetToSourcesMapping"]),
        "the corrected payload required name must be published"
    );
    let mapping = payload["properties"]["targetToSourcesMapping"]
        .as_object()
        .expect("the payload property for the mapping must exist");
    assert!(
        !mapping.is_empty(),
        "the bulk mapping property must be nonempty"
    );
    assert_eq!(payload["additionalProperties"], serde_json::json!(false));
    let mapping_def = defs
        .get("targetToSourcesMapping")
        .expect("the mapping definition is retained");
    assert!(
        mapping_def["required"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("issueIdsOrKeys")),
        "the corrected mapping required name must be published"
    );
    assert!(mapping_def["properties"]["issueIdsOrKeys"].is_object());
    assert_eq!(
        mapping_def["additionalProperties"],
        serde_json::json!(false)
    );

    let members = catalog
        .find_by_method_path(
            "put",
            "/rest/api/3/issuesecurityschemes/{schemeId}/level/{levelId}/member",
        )
        .expect("the security scheme member operation stays registered");
    let member_defs = members.input_schema["$defs"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    let request = member_defs
        .get("SecuritySchemeMembersRequest")
        .expect("the member request definition is retained");
    assert_eq!(request["required"], serde_json::json!(["members"]));
    assert!(request["properties"]["members"].is_object());
    assert_eq!(request["additionalProperties"], serde_json::json!(false));

    // No vendor typo string survives into any published schema.
    let serialized = serde_json::to_string(&specs).unwrap();
    for typo in [
        "targetToMultipleSourceMapping",
        "securitySchemeLevelMembers",
        "\"issueIdOrKeys\"",
    ] {
        assert!(
            !serialized.contains(typo),
            "{typo} must be gone from the patched published schemas"
        );
    }

    // The patched catalog still fits the stock broker budget.
    let bytes = catalog.serialized_tool_catalog_bytes();
    assert!(
        bytes < 900_000,
        "patched catalog serialized to {bytes} bytes, above the 900 KB ceiling"
    );
}

#[test]
fn patched_json_and_yaml_sources_are_equivalent() {
    harnx_core::require_nextest();
    let json_document = common::load_fixture(FIXTURE);
    let yaml_text = serde_yaml::to_string(&json_document.root).unwrap();
    let yaml_document =
        SpecDocument::from_yaml(yaml_text.as_bytes()).expect("the YAML rendering must parse");
    let json_patched = apply_spec_patches(
        &json_document,
        &witness_patches(),
        SpecPatchLimits::default(),
    )
    .unwrap();
    let yaml_patched = apply_spec_patches(
        &yaml_document,
        &witness_patches(),
        SpecPatchLimits::default(),
    )
    .unwrap();
    assert_eq!(
        json_patched.root, yaml_patched.root,
        "JSON and YAML sources must patch identically"
    );
    let json_catalog = Catalog::build(&json_patched, SERVER).unwrap();
    let yaml_catalog = Catalog::build(&yaml_patched, SERVER).unwrap();
    assert_eq!(json_catalog.inventory(), yaml_catalog.inventory());
    assert_eq!(
        serde_json::to_string(&json_catalog.tool_specs()).unwrap(),
        serde_json::to_string(&yaml_catalog.tool_specs()).unwrap(),
        "JSON and YAML patched documents must normalize identically"
    );
}

#[test]
fn patch_failure_fails_the_whole_catalog_atomically() {
    harnx_core::require_nextest();
    let document = common::load_fixture(FIXTURE);
    // Stage 4 breaks at runtime: the patched document must never reach
    // normalization, and the first three stages leave no partial effect.
    let mut patches = witness_patches();
    patches.push("error(\"late-stage-secret\")".to_string());
    let error = Catalog::build_patched(&document, SERVER, &patches, SpecPatchLimits::default())
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("spec patch failed")
            && error.contains("patch 4 of 4")
            && error.contains("failed during evaluation"),
        "{error}"
    );
    assert!(
        !error.contains("late-stage-secret"),
        "the failure must not echo the expression: {error}"
    );
    // The source document is untouched by a failed application.
    assert_eq!(
        document.root["components"]["schemas"]["IssueBulkMovePayload"]["required"],
        serde_json::json!(["targetToMultipleSourceMapping"])
    );
    // A host-effect patch is rejected for the real fixture too, without
    // echoing the expression.
    let secret = ".components.schemas = env.CREDENTIAL_PLACEHOLDER";
    let error = Catalog::build_patched(
        &document,
        SERVER,
        &[secret.to_string()],
        SpecPatchLimits::default(),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("did not compile"), "{error}");
    assert!(!error.contains("CREDENTIAL_PLACEHOLDER"), "{error}");
    assert!(!error.contains(".components"), "{error}");
}

#[test]
fn patched_invalid_document_revalidation_does_not_leak_document_content() {
    harnx_core::require_nextest();
    let document = common::load_fixture(FIXTURE);
    // The patch plants a secret sentinel in the version field — exactly
    // where the old re-validation error echoed the patched value — and
    // makes the document invalid in the process.
    const SENTINEL: &str = "hunter2-patch-sentinel-token";
    let patches = vec![format!(r#".openapi = "3.{SENTINEL}""#)];
    let error = Catalog::build_patched(&document, SERVER, &patches, SpecPatchLimits::default())
        .unwrap_err();
    let rendered = error.to_string();
    assert!(
        rendered.contains("spec patch failed") && rendered.contains("patch 1 of 1"),
        "the re-validation failure must be an indexed patch failure: {rendered}"
    );
    assert!(
        rendered.contains("uses a specification feature this crate does not support"),
        "the safe failure category must be preserved: {rendered}"
    );
    assert!(!rendered.contains(SENTINEL), "{rendered}");
    // The Debug render (error-chain formatting included) stays clean too.
    let debugged = format!("{error:?}");
    assert!(!debugged.contains(SENTINEL), "{debugged}");
}
