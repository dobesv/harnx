//! Normalization tests against authored Swagger 2.0 and OpenAPI 3.0 fixtures:
//! parameter merge and overrides, requiredness, recursion, composition,
//! nullability, credential protection, naming collisions, JSON/YAML
//! equivalence, and actionable diagnostics for unsupported constructs.

use crate::common;

use harnx_openapi_tools::{BodyInput, Catalog, ParamStyle, ProtectedParam, SpecDocument};
use serde_json::{json, Value};

fn swagger2() -> Catalog {
    Catalog::build(&common::load_fixture("swagger2.json"), "petstore").unwrap()
}

fn oas3() -> Catalog {
    Catalog::build(&common::load_fixture("oas3.json"), "things").unwrap()
}

#[test]
fn swagger2_host_basepath_and_schemes_are_documentation_only() {
    harnx_core::require_nextest();
    let catalog = swagger2();
    assert_eq!(
        catalog.server_metadata,
        vec!["https://petstore.example.com/v2".to_string()]
    );
    assert_eq!(catalog.title, "Petstore (authored fixture)");
}

#[test]
fn swagger2_query_parameters_carry_types_and_collection_formats() {
    harnx_core::require_nextest();
    let catalog = swagger2();
    let list_pets = catalog.find_operation("listPets").unwrap();
    let limit = &list_pets
        .query_params
        .iter()
        .find(|param| param.name == "limit")
        .expect("limit resolved from #/parameters");
    assert_eq!(limit.schema["type"], json!("integer"));
    assert_eq!(limit.schema["minimum"], json!(1));
    assert_eq!(limit.schema["default"], json!(20));
    let tags = &list_pets
        .query_params
        .iter()
        .find(|param| param.name == "tags")
        .expect("tags present");
    assert_eq!(tags.style, ParamStyle::Form { explode: false });
    // The scheme-named query API key is a protected input, not a model
    // argument, and the operation stays available.
    assert!(
        !list_pets
            .query_params
            .iter()
            .any(|param| param.name == "apiKey"),
        "spec-identified credentials are excluded from model arguments"
    );
    assert!(list_pets.protected_params.contains(&ProtectedParam {
        name: "apiKey".to_string(),
        scheme: "apiKeyQuery".to_string(),
        placement: harnx_openapi_tools::ApiKeyPlacement::Query,
    }));
    assert!(
        catalog
            .credential_diagnostics
            .iter()
            .any(|message| message.contains("apiKeyQuery") && message.contains("query")),
        "unsupported credential placement must produce a startup diagnostic: {:?}",
        catalog.credential_diagnostics
    );
}

#[test]
fn operation_parameters_override_path_parameters() {
    harnx_core::require_nextest();
    let catalog = swagger2();
    // The path item declares petId as integer and verbose as required; the
    // operation overrides both.
    let get_pet = catalog.find_operation("getPet").unwrap();
    let pet_id = &get_pet.path_params[0];
    assert_eq!(pet_id.schema["type"], json!("string"));
    let verbose = &get_pet.query_params[0];
    assert!(!verbose.required);
    assert!(
        !get_pet.input_schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|group| group == "query"),
        "an optional-only query group is not required"
    );
    // The delete operation inherits the path-level verbose parameter (still
    // required there) and adds its own optional header.
    let delete_pet = catalog.find_operation("deletePet").unwrap();
    assert_eq!(delete_pet.query_params[0].name, "verbose");
    assert!(delete_pet.query_params[0].required);
    assert!(
        delete_pet.input_schema["required"]
            .as_array()
            .unwrap()
            .contains(&json!("query")),
        "the inherited required verbose makes the query group required"
    );
    assert_eq!(delete_pet.header_params[0].name, "X-Request-Id");
    assert!(delete_pet.deprecated);
    assert!(delete_pet.description.contains("(deprecated)"));
}

#[test]
fn swagger2_body_parameter_becomes_direct_json_with_resolved_defs() {
    harnx_core::require_nextest();
    let catalog = swagger2();
    let create_pet = catalog.find_operation("createPet").unwrap();
    let body = create_pet.body.as_ref().unwrap();
    assert!(body.required);
    assert!(
        matches!(&body.input, BodyInput::Direct(schema) if schema["$ref"]
            == json!("#/$defs/Pet")),
        "the single JSON body publishes as the direct value"
    );
    let defs = create_pet.input_schema["$defs"].as_object().unwrap();
    assert!(defs.contains_key("Pet"));
    assert!(defs.contains_key("Category"));
    assert!(defs.contains_key("Tag"));
    // The recursive Category survives as a reference.
    assert_eq!(
        defs["Category"]["properties"]["parent"],
        json!({"$ref": "#/$defs/Category"})
    );
    // Pet keeps its requiredness and enum constraint.
    assert_eq!(defs["Pet"]["required"], json!(["name", "photoUrls"]));
    assert_eq!(
        defs["Pet"]["properties"]["status"]["enum"],
        json!(["available", "pending", "sold"])
    );
    assert!(create_pet.input_schema["required"]
        .as_array()
        .unwrap()
        .contains(&json!("body")));
}

#[test]
fn swagger2_file_form_data_becomes_multipart_parts() {
    harnx_core::require_nextest();
    let catalog = swagger2();
    let upload = catalog.find_operation("uploadPetImage").unwrap();
    let body = upload.body.as_ref().unwrap();
    assert!(body.required);
    let (labels, file): (Vec<_>, Vec<_>) = body
        .form_fields
        .iter()
        .partition(|field| field.name == "labels");
    assert!(file[0].required);
    assert_eq!(
        labels[0].collection_format,
        ParamStyle::Form { explode: true }
    );
    // A `type: file` formData field selects multipart: URL-encoded fields
    // cannot carry a file.
    assert_eq!(body.branches[0].media_type, "multipart/form-data");
    let BodyInput::Envelope(envelope) = &body.input else {
        panic!("multipart formData must publish an envelope");
    };
    assert_eq!(
        envelope["properties"]["content_type"]["enum"],
        json!(["multipart/form-data"])
    );
    assert_eq!(
        envelope["properties"]["encoding"]["enum"],
        json!(["multipart"])
    );
    // Part names stay constrained to the declared fields, and the branch
    // schema carries binary strings — never the invalid `type: "file"`.
    assert_eq!(
        envelope["properties"]["parts"]["items"]["properties"]["name"]["enum"],
        json!(["file", "labels"])
    );
    assert_eq!(
        body.branches[0].source_schema["properties"]["file"],
        json!({"type": "string", "format": "binary"})
    );
    // The scheme-named X-API-Key header is protected, never a model argument.
    assert!(upload.header_params.is_empty());
    assert_eq!(upload.protected_params[0].scheme, "apiKeyHeader");
    assert!(upload.protected_params[0]
        .name
        .eq_ignore_ascii_case("x-api-key"));
}

#[test]
fn duplicate_operation_ids_get_distinct_stable_names() {
    harnx_core::require_nextest();
    let catalog = swagger2();
    let first = catalog.find_by_method_path("get", "/admin/dupOp").unwrap();
    let second = catalog.find_by_method_path("get", "/admin/dupOp2").unwrap();
    assert_eq!(first.raw_tool_name, "dupOp");
    assert_ne!(first.raw_tool_name, second.raw_tool_name);
    assert!(second.raw_tool_name.starts_with("dupOp_"));
    assert_eq!(second.raw_tool_name.len(), "dupOp_".len() + 8);
}

#[test]
fn literal_operation_id_cannot_shadow_a_generated_suffix_or_its_schema() {
    harnx_core::require_nextest();
    let generated_path = "/generated";
    let generated_hash =
        harnx_openapi_tools::compact::short_hash(&format!("post {generated_path}"));
    let literal_id = format!("dupOp_{generated_hash}");
    let operation = |operation_id: String, min_length| {
        json!({
            "operationId": operation_id,
            "parameters": [{
                "name": "value",
                "in": "query",
                "required": false,
                "schema": {"type": "string", "minLength": min_length}
            }],
            "responses": {"200": {"description": "ok"}}
        })
    };
    let document = SpecDocument::from_value(json!({
        "openapi": "3.0.3",
        "info": {"title": "Names", "version": "1"},
        "paths": {
            "/first": {"get": operation("dupOp".to_string(), 1)},
            "/literal": {"get": operation(literal_id.clone(), 3)},
            "/generated": {"post": operation("dupOp".to_string(), 7)}
        }
    }))
    .unwrap();
    let catalog = Catalog::build(&document, "names").unwrap();
    let first = catalog.find_by_method_path("get", "/first").unwrap();
    let literal = catalog.find_by_method_path("get", "/literal").unwrap();
    let generated = catalog.find_by_method_path("post", generated_path).unwrap();
    assert_eq!(first.raw_tool_name, "dupOp");
    assert_eq!(literal.raw_tool_name, literal_id);
    assert!(
        generated
            .raw_tool_name
            .starts_with(&format!("dupOp_{generated_hash}_")),
        "{}",
        generated.raw_tool_name
    );
    assert!(generated.raw_tool_name.len() <= 64 - "names_".len());
    for operation in [first, literal, generated] {
        assert_eq!(
            catalog
                .find_operation(&operation.raw_tool_name)
                .map(|found| found.path.as_str()),
            Some(operation.path.as_str())
        );
    }
    let names = catalog
        .tool_specs()
        .into_iter()
        .map(|tool| tool.name)
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(names.len(), catalog.operations.len());

    for (operation, minimum, accepted, rejected) in [
        (first, 1, "x", ""),
        (literal, 3, "xxx", "x"),
        (generated, 7, "1234567", "short"),
    ] {
        let raw_schema = &operation.input_schema;
        assert_eq!(
            raw_schema["properties"]["query"]["properties"]["value"]["minLength"],
            minimum
        );
        let core_schema = harnx_core::json_schema::JsonSchema::from_tool_schema(raw_schema.clone())
            .unwrap()
            .into_value();
        let raw_validator = jsonschema::validator_for(raw_schema).unwrap();
        let core_validator = jsonschema::validator_for(&core_schema).unwrap();
        let valid = json!({"query": {"value": accepted}});
        let invalid = json!({"query": {"value": rejected}});
        assert!(raw_validator.is_valid(&valid));
        assert!(!raw_validator.is_valid(&invalid));
        assert!(core_validator.is_valid(&valid));
        assert!(!core_validator.is_valid(&invalid));
    }
}

#[test]
fn swagger2_json_and_yaml_fixtures_normalize_identically() {
    harnx_core::require_nextest();
    let json_document = common::load_fixture("swagger2.json");
    let yaml_document = common::load_fixture("swagger2.yaml");
    let json_catalog = Catalog::build(&json_document, "petstore").unwrap();
    let yaml_catalog = Catalog::build(&yaml_document, "petstore").unwrap();
    assert_eq!(json_catalog.inventory(), yaml_catalog.inventory());
    assert_eq!(
        serde_json::to_string(&json_catalog.tool_specs()).unwrap(),
        serde_json::to_string(&yaml_catalog.tool_specs()).unwrap()
    );
}

#[test]
fn oas3_request_body_ref_resolves_and_requiredness_matches_the_wire() {
    harnx_core::require_nextest();
    let catalog = oas3();
    let replace = catalog.find_operation("replaceThing").unwrap();
    let body = replace.body.as_ref().unwrap();
    assert!(body.required);
    assert!(
        matches!(&body.input, BodyInput::Direct(schema) if schema["$ref"] == json!("#/$defs/Thing"))
    );
    // Requiredness: path (thingId) and body groups required, verbose
    // overridden to optional.
    assert_eq!(replace.input_schema["required"], json!(["path", "body"]));
    assert!(replace
        .path_params
        .iter()
        .any(|param| param.name == "thingId" && param.schema["minLength"] == json!(1)));
    // No operation-level security: the global requirement applies.
    assert_eq!(replace.security, vec!["oauth".to_string()]);
    // Global security applies where the operation declares none.
    assert_eq!(
        catalog.find_operation("listThings").unwrap().security,
        vec!["oauth".to_string()]
    );
    // Operation-level security overrides the global requirement list.
    assert_eq!(
        catalog.find_operation("getThing").unwrap().security,
        vec!["oauth".to_string(), "basic".to_string()]
    );
}

#[test]
fn oas3_nullable_enum_and_map_schemas_are_preserved() {
    harnx_core::require_nextest();
    let catalog = oas3();
    let create = catalog.find_operation("createThing").unwrap();
    let defs = create.input_schema["$defs"].as_object().unwrap();
    let count = &defs["Thing"]["properties"]["count"];
    assert_eq!(count["nullable"], json!(true));
    assert_eq!(count["minimum"], json!(0));
    assert_eq!(
        defs["Thing"]["properties"]["status"]["enum"],
        json!(["draft", "live", "archived"])
    );
    assert_eq!(
        defs["Thing"]["properties"]["meta"]["additionalProperties"],
        json!({"type": "string"})
    );
    assert_eq!(defs["Thing"]["required"], json!(["name"]));
    // Recursive Node stays a reference.
    assert_eq!(
        defs["Node"]["properties"]["children"]["items"],
        json!({"$ref": "#/$defs/Node"})
    );
    // allOf composition is preserved verbatim.
    let merge = catalog.find_operation("mergeThing").unwrap();
    let merged_defs = merge.input_schema["$defs"].as_object().unwrap();
    assert_eq!(
        merged_defs["ThingUpdate"]["allOf"][0],
        json!({"$ref": "#/$defs/Thing"})
    );
    assert_eq!(
        merged_defs["ThingUpdate"]["allOf"][1]["properties"]["revision"]["minimum"],
        json!(1)
    );
}

#[test]
fn oas3_media_variants_publish_the_documented_envelope_shapes() {
    harnx_core::require_nextest();
    let catalog = oas3();
    // json-patch: direct array despite the object schema.
    let patch = catalog.find_operation("updateThing").unwrap();
    let BodyInput::Direct(patch_schema) = &patch.body.as_ref().unwrap().input else {
        panic!("json-patch body must be direct");
    };
    assert_eq!(patch_schema["type"], json!("array"));
    assert_eq!(
        patch_schema["items"]["properties"]["op"]["enum"],
        json!(["add", "remove", "replace", "move", "copy", "test"])
    );

    // json + text/plain: envelope with both encodings.
    let name = catalog.find_operation("setThingName").unwrap();
    let BodyInput::Envelope(envelope) = &name.body.as_ref().unwrap().input else {
        panic!("two media entries must publish an envelope");
    };
    let json = common::media_variant(envelope, "json", "application/json");
    let text = common::media_variant(envelope, "text", "text/plain");
    assert_eq!(json["properties"]["value"]["type"], "string");
    assert_eq!(text["properties"]["text"]["type"], "string");

    // Wildcard and multipart use the shared source schema independently.
    let columns = catalog.find_operation("setThingColumns").unwrap();
    let BodyInput::Envelope(envelope) = &columns.body.as_ref().unwrap().input else {
        panic!("wildcard bodies publish an envelope");
    };
    let json = common::media_variant(envelope, "json", "application/json");
    assert_eq!(json["properties"]["value"]["$ref"], "#/$defs/ColumnSpec");
    let form = common::media_variant(envelope, "form", "application/x-www-form-urlencoded");
    assert_eq!(
        form["properties"]["fields"]["items"]["properties"]["name"]["enum"],
        json!(["columns"])
    );
    let multipart = common::media_variant(envelope, "multipart", "multipart/form-data");
    assert_eq!(
        multipart["properties"]["parts"]["items"]["properties"]["name"]["enum"],
        json!(["columns"])
    );

    // Binary wildcard has a viable base64 branch and never offers JSON.
    let icon = catalog.find_operation("setIcon").unwrap();
    let BodyInput::Envelope(envelope) = &icon.body.as_ref().unwrap().input else {
        panic!("binary wildcard publishes an envelope");
    };
    let binary = common::media_variant(envelope, "base64", "image/png");
    assert!(binary["properties"]["base64"].is_object());
    assert!(envelope["anyOf"]
        .as_array()
        .unwrap()
        .iter()
        .all(|variant| variant["properties"].get("value").is_none()));

    // single concrete non-JSON type: base64 envelope with the concrete enum.
    let archive = catalog.find_operation("archiveThing").unwrap();
    let BodyInput::Envelope(envelope) = &archive.body.as_ref().unwrap().input else {
        panic!("single binary body publishes an envelope");
    };
    assert_eq!(
        envelope["properties"]["content_type"]["enum"],
        json!(["application/octet-stream"])
    );
    assert_eq!(
        envelope["properties"]["encoding"]["enum"],
        json!(["base64"])
    );
}

#[test]
fn oas3_query_enum_survives_and_credentials_are_protected() {
    harnx_core::require_nextest();
    let catalog = oas3();
    let nodes = catalog.find_operation("getNodes").unwrap();
    // X-Things-Token (header scheme) and apiToken (query scheme) are both
    // protected; the ordinary `mode` parameter is not mistaken for one.
    assert!(nodes.header_params.is_empty());
    assert!(!nodes
        .query_params
        .iter()
        .any(|param| param.name == "apiToken"));
    let mode = nodes
        .query_params
        .iter()
        .find(|param| param.name == "mode")
        .unwrap();
    assert_eq!(mode.schema["enum"], json!(["flat", "deep"]));
    assert!(mode.required);
    assert_eq!(nodes.protected_params.len(), 2);
    assert!(
        catalog
            .credential_diagnostics
            .iter()
            .any(|message| message.contains("tokenQuery")),
        "query credential placement must be diagnosed: {:?}",
        catalog.credential_diagnostics
    );
    // The query group is still required because `mode` is required.
    assert!(nodes.input_schema["required"]
        .as_array()
        .unwrap()
        .contains(&json!("query")));
}

#[test]
fn self_contradictory_schemas_are_preserved_verbatim_with_a_diagnostic() {
    harnx_core::require_nextest();
    // A required name with no property and additionalProperties: false
    // rejects every request, including the vendor's own examples. The
    // contradiction is NOT repaired: the schema is kept verbatim and a
    // precise diagnostic is reported. Resolving vendor typos is a planning
    // decision; weakening the catalog to keep tests green is not authorized.
    let catalog = build_from_json(
        json!({
            "openapi": "3.0.1",
            "paths": {"/broken": {"post": {
                "operationId": "createBroken",
                "requestBody": {"required": true, "content": {"application/json": {
                    "schema": {
                        "type": "object",
                        "required": ["ghost"],
                        "properties": {"real": {"type": "string"}},
                        "additionalProperties": false
                    }
                }}}
            }}}
        }),
        "srv",
    )
    .unwrap();
    let operation = catalog.find_operation("createBroken").unwrap();
    let body = operation.body.as_ref().unwrap();
    let BodyInput::Direct(schema) = &body.input else {
        panic!("single JSON body must publish directly");
    };
    // Kept verbatim: additionalProperties stays false, the typo'd required
    // name stays required. The operation stays registered.
    assert_eq!(schema["additionalProperties"], json!(false));
    assert_eq!(schema["required"], json!(["ghost"]));
    assert_eq!(schema["properties"]["real"], json!({"type": "string"}));
    assert!(
        catalog
            .schema_diagnostics
            .iter()
            .any(|message| message.contains("POST /broken")
                && message.contains("ghost")
                && message.contains("kept verbatim")
                && message.contains("cannot construct a valid request")),
        "the contradiction must be reported as a startup diagnostic: {:?}",
        catalog.schema_diagnostics
    );
    // The pinned vendor typo leaves the operation registered but
    // unsatisfiable: no request validates, which is exactly why the
    // all-constructible acceptance stays blocked pending a plan decision.
    let validator = jsonschema::validator_for(&operation.input_schema).unwrap();
    assert!(validator.validate(&json!({"body": {"real": "x"}})).is_err());
    assert!(validator
        .validate(&json!({"body": {"ghost": "x"}}))
        .is_err());
}

#[test]
fn authored_fixture_schemas_pass_core_conversion_and_validate_synthesized_requests() {
    harnx_core::require_nextest();
    // The authored fixtures are fully satisfiable, so — unlike the pinned
    // Jira contradictions — every operation's published schema must run the
    // actual core conversion and validate a synthesized request.
    for catalog in [swagger2(), oas3()] {
        for spec in catalog.tool_specs() {
            harnx_core::json_schema::JsonSchema::from_tool_schema(spec.input_schema)
                .unwrap_or_else(|error| panic!("{}: core conversion failed: {error}", spec.name));
        }
        for operation in &catalog.operations {
            let defs = operation
                .input_schema
                .get("$defs")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let arguments = common::synthesize(&operation.input_schema, &defs);
            let validator =
                jsonschema::validator_for(&operation.input_schema).unwrap_or_else(|error| {
                    panic!("{}: schema does not compile: {error}", operation.context())
                });
            validator.validate(&arguments).unwrap_or_else(|error| {
                panic!(
                    "{}: synthesized arguments {arguments} do not satisfy the published \
                     schema: {error}",
                    operation.context()
                )
            });
        }
    }
}

fn build_from_json(value: Value, server: &str) -> harnx_openapi_tools::SpecResult<Catalog> {
    let document = SpecDocument::from_value(value)?;
    Catalog::build(&document, server)
}

#[test]
fn unsupported_constructs_fail_with_located_diagnostics() {
    harnx_core::require_nextest();
    // OpenAPI 3.1 is rejected with guidance.
    let error = build_from_json(
        json!({"openapi": "3.1.0", "paths": {"/a": {"get": {"operationId": "a"}}}}),
        "srv",
    )
    .unwrap_err();
    assert!(error.to_string().contains("3.1"), "{error}");
    assert!(error.to_string().contains("3.0"), "{error}");

    // External schema references are rejected at the operation that uses them.
    let error = build_from_json(
        json!({"openapi": "3.0.1", "paths": {"/upload": {"post": {
            "operationId": "upload",
            "requestBody": {"content": {"application/json": {
                "schema": {"$ref": "https://example.com/schema.json"}
            }}}
        }}}}),
        "srv",
    )
    .unwrap_err();
    let message = error.to_string();
    assert!(message.contains("POST /upload"), "{message}");
    assert!(
        message.contains("https://example.com/schema.json"),
        "{message}"
    );

    // Cookie parameters cannot be sent at all.
    let error = build_from_json(
        json!({"openapi": "3.0.1", "paths": {"/a": {"get": {
            "operationId": "a",
            "parameters": [{"name": "session", "in": "cookie", "schema": {"type": "string"}}]
        }}}}),
        "srv",
    )
    .unwrap_err();
    assert!(error.to_string().contains("cookie"), "{error}");

    // Unsupported OAS3 serialization styles.
    let error = build_from_json(
        json!({"openapi": "3.0.1", "paths": {"/a": {"get": {
            "operationId": "a",
            "parameters": [{"name": "f", "in": "query", "style": "deepObject", "schema": {"type": "object"}}]
        }}}}),
        "srv",
    )
    .unwrap_err();
    assert!(error.to_string().contains("deepObject"), "{error}");

    // Unsupported Swagger 2.0 collection formats.
    let error = build_from_json(
        json!({"swagger": "2.0", "paths": {"/a": {"get": {
            "operationId": "a",
            "parameters": [{"name": "ids", "in": "query", "type": "array", "collectionFormat": "ssv"}]
        }}}}),
        "srv",
    )
    .unwrap_err();
    assert!(error.to_string().contains("ssv"), "{error}");

    // A path template placeholder without a parameter, and vice versa.
    let error = build_from_json(
        json!({"openapi": "3.0.1", "paths": {"/a/{id}": {"get": {"operationId": "a"}}}}),
        "srv",
    )
    .unwrap_err();
    assert!(error.to_string().contains("id"), "{error}");
    let error = build_from_json(
        json!({"openapi": "3.0.1", "paths": {"/a": {"get": {
            "operationId": "a",
            "parameters": [{"name": "id", "in": "path", "required": true, "schema": {"type": "string"}}]
        }}}}),
        "srv",
    )
    .unwrap_err();
    assert!(error.to_string().contains("path template"), "{error}");

    // Security requirements naming undeclared schemes are invalid.
    let error = build_from_json(
        json!({"openapi": "3.0.1", "paths": {"/a": {"get": {
            "operationId": "a", "security": [{"ghost": []}]
        }}}}),
        "srv",
    )
    .unwrap_err();
    assert!(error.to_string().contains("ghost"), "{error}");

    // A request body with no media entries cannot construct a request.
    let error = build_from_json(
        json!({"openapi": "3.0.1", "paths": {"/a": {"post": {
            "operationId": "a", "requestBody": {"content": {}}
        }}}}),
        "srv",
    )
    .unwrap_err();
    assert!(error.to_string().contains("media"), "{error}");
}

#[test]
fn server_name_budget_and_long_names_are_configuration_and_hash_handled() {
    harnx_core::require_nextest();
    // A server name that cannot leave a usable raw budget fails up front.
    let error =
        build_from_json(json!({"swagger": "2.0", "paths": {}}), &"x".repeat(40)).unwrap_err();
    assert!(error.to_string().contains("--name"), "{error}");

    // A long custom prefix still budgets: names fit under it.
    let catalog = build_from_json(
        json!({
            "openapi": "3.0.1",
            "paths": {"/very/long/{id}/path/segments/that/keep/going/and/going": {
                "get": {
                    "operationId": "anExtremelyLongOperationIdentifierForBudgetTesting",
                    "parameters": [
                        {"name": "id", "in": "path", "required": true,
                         "schema": {"type": "string"}}
                    ]
                }
            }}
        }),
        &"x".repeat(39),
    )
    .unwrap();
    let operation = &catalog.operations[0];
    let prefix_len = 39 + 1;
    assert!(
        operation.raw_tool_name.len() <= harnx_openapi_tools::MAX_MODEL_TOOL_NAME_LEN - prefix_len,
        "{}",
        operation.raw_tool_name
    );
    assert!(operation.raw_tool_name.ends_with(&format!(
        "_{}",
        harnx_openapi_tools::compact::short_hash(
            "get /very/long/{id}/path/segments/that/keep/going/and/going"
        )
    )));
}

#[test]
fn head_options_and_trace_operations_normalize_like_any_other_method() {
    harnx_core::require_nextest();
    let catalog = oas3();
    for (method, name) in [
        ("head", "headThing"),
        ("options", "optionsThing"),
        ("trace", "traceThing"),
    ] {
        let operation = catalog
            .find_by_method_path(method, "/things/{thingId}")
            .unwrap_or_else(|| panic!("{method} operation missing from catalog"));
        assert_eq!(operation.method, method);
        assert_eq!(operation.raw_tool_name, name);
        assert_eq!(operation.path_params[0].name, "thingId");
        // head and options are safe; trace echoes whatever it receives.
        assert_eq!(
            operation.is_read_only(),
            method != "trace",
            "read_only hint for {method}"
        );
        let spec = catalog
            .tool_specs()
            .into_iter()
            .find(|spec| spec.name == name)
            .unwrap_or_else(|| panic!("no ToolSpec for {name}"));
        assert_eq!(spec.read_only_hint, method != "trace");
        assert_eq!(spec.idempotent_hint, method != "trace");
    }
}

#[test]
fn unknown_path_item_method_fails_instead_of_being_skipped() {
    harnx_core::require_nextest();
    // A method outside the supported set is diagnosed, never silently
    // dropped from the catalog.
    let error = build_from_json(
        json!({"openapi": "3.0.1", "paths": {"/a": {
            "get": {"operationId": "a"},
            "connect": {"operationId": "tunnel"}
        }}}),
        "srv",
    )
    .unwrap_err();
    let message = error.to_string();
    assert!(message.contains("connect"), "{message}");
    assert!(message.contains("/a"), "{message}");

    // Non-operation path-item fields and vendor extensions stay legal.
    build_from_json(
        json!({"openapi": "3.0.1", "paths": {"/a": {
            "summary": "a path",
            "description": "its description",
            "servers": [],
            "x-vendor": 7,
            "get": {"operationId": "a"}
        }}}),
        "srv",
    )
    .unwrap();
}
