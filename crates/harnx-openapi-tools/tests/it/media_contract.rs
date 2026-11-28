use harnx_core::json_schema::JsonSchema;
use harnx_openapi_tools::{
    select_media_branch, Catalog, MediaBranch, NormalizedOperation, SpecDocument,
};
use serde_json::{json, Map, Value};

fn catalog(content: &[(&str, Value)]) -> Catalog {
    Catalog::build(&oas(content), "media").unwrap()
}

fn oas(content: &[(&str, Value)]) -> SpecDocument {
    let entries: Map<_, _> = content
        .iter()
        .map(|(key, schema)| (key.to_string(), json!({"schema": schema})))
        .collect();
    SpecDocument::from_value(json!({
        "openapi": "3.0.3", "info": {"title": "media", "version": "1"},
        "paths": {"/media": {"post": {"requestBody": {"required": true, "content": entries}}}},
        "components": {"schemas": {
            "Strict": named("strict"),
            "Bytes": {"type": "string", "format": "binary"}
        }}
    }))
    .unwrap()
}

fn named(name: &str) -> Value {
    json!({"type": "object", "required": [name], "additionalProperties": false,
        "properties": {(name): {"type": "string"}, "optional": {"type": "string"}}})
}

pub(super) fn validators(operation: &NormalizedOperation) -> [jsonschema::Validator; 2] {
    let raw = &operation.input_schema;
    let core = JsonSchema::from_tool_schema(raw.clone()).unwrap();
    [
        jsonschema::validator_for(raw).unwrap(),
        jsonschema::validator_for(core.as_value()).unwrap(),
    ]
}

pub(super) fn check(validators: &[jsonschema::Validator; 2], args: &Value, valid: bool) {
    for (label, validator) in ["raw", "core"].into_iter().zip(validators) {
        let result = validator.validate(args);
        assert_eq!(result.is_ok(), valid, "{label} schema, {args}: {result:?}");
    }
}

fn selected<'a>(operation: &'a NormalizedOperation, media: &str) -> Option<&'a str> {
    select_media_branch(&operation.body.as_ref().unwrap().branches, media)
        .map(|branch| branch.media_type.as_str())
}

fn envelope(media: &str, encoding: &str, payload: &str, value: Value) -> Value {
    json!({"body": {"content_type": media, "encoding": encoding, (payload): value}})
}

#[test]
fn exact_form_and_multipart_win_with_distinct_names_in_either_order() {
    harnx_core::require_nextest();
    let form = "application/x-www-form-urlencoded";
    let multipart = "multipart/form-data";
    let mut declarations = vec![
        ("*/*", named("global")),
        ("multipart/*", named("range")),
        (form, named("exact_field")),
        (multipart, named("exact_part")),
    ];
    for _ in 0..2 {
        let catalog = catalog(&declarations);
        let op = &catalog.operations[0];
        let validate = validators(op);
        check_form(&validate, form);
        check_parts(&validate, multipart, "exact_part", "range");
        check_parts(&validate, "multipart/mixed", "range", "global");
        assert_eq!(selected(op, form), Some(form));
        assert_eq!(selected(op, multipart), Some(multipart));
        assert_eq!(selected(op, "multipart/mixed"), Some("multipart/*"));
        declarations.reverse();
    }
}

fn check_form(validate: &[jsonschema::Validator; 2], media: &str) {
    let fields =
        json!([{"name": "exact_field", "value": "one"}, {"name": "exact_field", "value": "two"}]);
    check(validate, &envelope(media, "form", "fields", fields), true);
    for name in ["global", "optional"] {
        check(
            validate,
            &envelope(
                media,
                "form",
                "fields",
                json!([{"name": name, "value": "x"}]),
            ),
            false,
        );
    }
}

fn check_parts(validate: &[jsonschema::Validator; 2], media: &str, good: &str, bad: &str) {
    let parts = json!([{"name": good, "text": "one"}, {"name": good, "base64": "YQ==", "filename": "a.txt"}]);
    check(
        validate,
        &envelope(media, "multipart", "parts", parts),
        true,
    );
    for part in [
        json!({"name": bad, "text": "x"}),
        json!({"name": "optional", "text": "x"}),
        json!({"name": good}),
        json!({"name": good, "text": "x", "base64": "eA=="}),
    ] {
        check(
            validate,
            &envelope(media, "multipart", "parts", json!([part])),
            false,
        );
    }
}

#[test]
fn exact_type_and_global_json_constraints_do_not_bleed_in_either_order() {
    harnx_core::require_nextest();
    let mut declarations = vec![
        ("*/*", named("global")),
        ("ApPlIcAtIoN/*", named("range")),
        (
            "Application/JSON; charset=UTF-8",
            json!({"$ref": "#/components/schemas/Strict"}),
        ),
    ];
    for _ in 0..2 {
        let catalog = catalog(&declarations);
        let op = &catalog.operations[0];
        let validate = validators(op);
        for (media, branch, good, bad) in [
            ("application/json", "application/json", "strict", "range"),
            (
                "application/vnd.body+json",
                "application/*",
                "range",
                "global",
            ),
            ("example/custom+json", "*/*", "global", "strict"),
        ] {
            assert_eq!(selected(op, media), Some(branch));
            check(
                &validate,
                &envelope(media, "json", "value", json!({(good): "x"})),
                true,
            );
            check(
                &validate,
                &envelope(media, "json", "value", json!({(bad): "x"})),
                false,
            );
        }
        for spelling in [
            "Application/JSON",
            "application/json; charset=utf-8",
            "application/json\n",
            " application/json",
        ] {
            assert_eq!(selected(op, spelling), None);
            check(
                &validate,
                &envelope(spelling, "json", "value", json!({"global": "x"})),
                false,
            );
            check(
                &validate,
                &envelope(spelling, "json", "value", json!({"strict": "x"})),
                false,
            );
        }
        check(
            &validate,
            &envelope(
                "application/json-patch+json",
                "json",
                "value",
                json!([{"op": "remove", "path": "/name"}]),
            ),
            true,
        );
        check(
            &validate,
            &envelope(
                "application/json-patch+json",
                "json",
                "value",
                json!({"range": "x"}),
            ),
            false,
        );
        declarations.reverse();
    }
}

#[test]
fn only_matching_type_ranges_and_concrete_encoders_are_accepted() {
    harnx_core::require_nextest();
    let catalog = catalog(&[
        ("Text/*", json!({})),
        ("Multipart/*", json!({})),
        ("ImAgE/*", json!({})),
        ("X.A+B/*", json!({})),
    ]);
    let op = &catalog.operations[0];
    let validate = validators(op);
    for (media, branch, encoding, key, value) in [
        ("text/plain", "text/*", "text", "text", json!("hi")),
        (
            "multipart/mixed",
            "multipart/*",
            "multipart",
            "parts",
            json!([{"name": "field", "text": "hi"}]),
        ),
        ("image/png", "image/*", "base64", "base64", json!("YQ==")),
        ("x.a+b/data", "x.a+b/*", "base64", "base64", json!("YQ==")),
    ] {
        assert_eq!(selected(op, media), Some(branch));
        check(&validate, &envelope(media, encoding, key, value), true);
        check(
            &validate,
            &json!({"body": {"content_type": media, "encoding": encoding}}),
            false,
        );
    }
    for invalid in [
        "application/octet-stream",
        "image/*",
        "image/png/extra",
        "image/png\n",
        "IMAGE/PNG",
        "image/",
        "xqaab/data",
    ] {
        assert_eq!(selected(op, invalid), None);
        check(
            &validate,
            &envelope(invalid, "base64", "base64", json!("YQ==")),
            false,
        );
    }
    check(
        &validate,
        &envelope("text/plain", "base64", "base64", json!("YQ==")),
        false,
    );
    check(
        &validate,
        &envelope("image/png", "text", "text", json!("wrong")),
        false,
    );
}

#[test]
fn binary_range_does_not_offer_json_or_fall_through_when_json_would_match() {
    harnx_core::require_nextest();
    for binary in [
        json!({"type": "string", "format": "binary"}),
        json!({"$ref": "#/components/schemas/Bytes"}),
    ] {
        let catalog = catalog(&[
            ("*/*", binary.clone()),
            (
                "application/json",
                json!({"$ref": "#/components/schemas/Strict"}),
            ),
        ]);
        let op = &catalog.operations[0];
        let validate = validators(op);
        assert_eq!(selected(op, "application/vnd.probe+json"), None);
        check(
            &validate,
            &envelope("application/vnd.probe+json", "json", "value", json!("YQ==")),
            false,
        );
        check(
            &validate,
            &envelope(
                "application/vnd.probe+json",
                "base64",
                "base64",
                json!("YQ=="),
            ),
            false,
        );
        check(
            &validate,
            &envelope("image/png", "base64", "base64", json!("YQ==")),
            true,
        );
        check(
            &validate,
            &envelope("application/json", "base64", "base64", json!("YQ==")),
            false,
        );
        check(
            &validate,
            &json!({"body": {"content_type": "application/json", "encoding": "json"}}),
            false,
        );
        let mut mixed = envelope("application/json", "json", "value", json!({"strict": "x"}));
        check(&validate, &mixed, true);
        mixed["body"]["base64"] = json!("YQ==");
        check(&validate, &mixed, false);
        let catalog = self::catalog(&[("*/*", named("global")), ("application/*", binary)]);
        let op = &catalog.operations[0];
        assert_eq!(selected(op, "application/json"), None);
        check(
            &validators(op),
            &envelope("application/json", "json", "value", json!({"global": "x"})),
            false,
        );
    }
}

#[test]
fn canonical_duplicates_and_malformed_ranges_get_located_diagnostics() {
    harnx_core::require_nextest();
    for (a, b, canonical) in [
        (
            "Application/JSON; charset=utf-8",
            "application/json",
            "application/json",
        ),
        ("TEXT/*", "text/*", "text/*"),
    ] {
        let error = Catalog::build(&oas(&[(a, named("a")), (b, named("b"))]), "media")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("POST /media")
                && error.contains("ambiguous media")
                && error.contains(canonical),
            "{error}"
        );
    }
    for invalid in ["*/plain", "image/p*", "text/plain/bad", "unsplit", "image/"] {
        let error = Catalog::build(&oas(&[(invalid, json!({}))]), "media")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("POST /media") && error.contains(invalid),
            "{error}"
        );
    }
    let empty = Catalog::build(&oas(&[]), "media").unwrap_err().to_string();
    assert!(
        empty.contains("POST /media") && empty.contains("no media entries"),
        "{empty}"
    );
}

fn swagger(consumes: &[&str], parameter: Value) -> SpecDocument {
    SpecDocument::from_value(json!({
        "swagger": "2.0", "info": {"title": "media", "version": "1"},
        "consumes": ["image/gif"],
        "paths": {"/media": {"post": {"consumes": consumes, "parameters": [parameter]}}}
    }))
    .unwrap()
}

#[test]
fn swagger_body_consumes_preserves_json_patch_text_and_json_choices() {
    harnx_core::require_nextest();
    let spec = swagger(
        &[
            "Text/Plain; charset=UTF-8",
            "application/json-patch+json",
            "application/json",
        ],
        json!({"in": "body", "name": "body", "required": true, "schema": named("strict")}),
    );
    let catalog = Catalog::build(&spec, "media").unwrap();
    let op = &catalog.operations[0];
    let validate = validators(op);
    assert_eq!(op.body.as_ref().unwrap().branches.len(), 3);
    check(
        &validate,
        &envelope("text/plain", "text", "text", json!("x")),
        true,
    );
    check(
        &validate,
        &envelope(
            "application/json-patch+json",
            "json",
            "value",
            json!([{"op": "remove", "path": "/a"}]),
        ),
        true,
    );
    check(
        &validate,
        &envelope("application/json", "json", "value", json!({"strict": "x"})),
        true,
    );
    check(
        &validate,
        &envelope("image/gif", "base64", "base64", json!("YQ==")),
        false,
    );
}

#[test]
fn swagger_form_consumes_keeps_compatible_alternatives_and_rejects_file_conflict() {
    harnx_core::require_nextest();
    let consumes = ["application/x-www-form-urlencoded", "multipart/form-data"];
    let spec = swagger(
        &consumes,
        json!({"in": "formData", "name": "message", "type": "string", "required": true}),
    );
    let catalog = Catalog::build(&spec, "media").unwrap();
    let op = &catalog.operations[0];
    let validate = validators(op);
    check(
        &validate,
        &envelope(
            consumes[0],
            "form",
            "fields",
            json!([{"name": "message", "value": "x"}]),
        ),
        true,
    );
    check(
        &validate,
        &envelope(
            consumes[1],
            "multipart",
            "parts",
            json!([{"name": "message", "text": "x"}]),
        ),
        true,
    );
    let bad = swagger(
        &consumes,
        json!({"in": "formData", "name": "upload", "type": "file", "required": true}),
    );
    let error = Catalog::build(&bad, "media").unwrap_err().to_string();
    assert!(
        error.contains("POST /media")
            && error.contains(consumes[0])
            && error.contains("file fields"),
        "{error}"
    );
}

#[test]
fn direct_json_uses_canonical_advertised_identity_and_requires_body() {
    harnx_core::require_nextest();
    let catalog = catalog(&[("Application/JSON; charset=utf-8", json!({"type": "string"}))]);
    let op = &catalog.operations[0];
    let validate = validators(op);
    check(&validate, &json!({"body": "value"}), true);
    check(&validate, &json!({"body": ["value"]}), false);
    check(&validate, &json!({}), false);
    let branches: &[MediaBranch] = &op.body.as_ref().unwrap().branches;
    assert_eq!(branches[0].media_type, "application/json");
}

#[test]
fn exact_and_range_text_apply_different_source_constraints() {
    harnx_core::require_nextest();
    let catalog = catalog(&[
        ("text/*", json!({"type": "string", "enum": ["range"]})),
        ("text/plain", json!({"type": "string", "enum": ["exact"]})),
    ]);
    let op = &catalog.operations[0];
    let validate = validators(op);
    for (media, expected, good, bad) in [
        ("text/plain", "text/plain", "exact", "range"),
        ("text/csv", "text/*", "range", "exact"),
    ] {
        assert_eq!(selected(op, media), Some(expected));
        check(
            &validate,
            &envelope(media, "text", "text", json!(good)),
            true,
        );
        check(
            &validate,
            &envelope(media, "text", "text", json!(bad)),
            false,
        );
    }
}
