//! Focused regressions for credential-input classification (review findings
//! B5 and B6 of checkpoint 515b5bc25):
//!
//! - B5: reserved HTTP credential headers (`Authorization`,
//!   `Proxy-Authorization`, `Cookie`) are protected inputs at the header
//!   location, case-insensitively, with or without a matching scheme; the
//!   same names stay ordinary model arguments at other locations.
//! - B6: a security scheme declared as a Reference Object resolves through
//!   its local target before classification; dangling, non-local and cyclic
//!   scheme references fail with scheme/ref context instead of silently
//!   passing name validation.

use harnx_openapi_tools::{
    ApiKeyPlacement, Catalog, ProtectedParam, SpecDocument, RESERVED_HEADER_SCHEME,
};
use serde_json::{json, Value};

fn build(root: Value) -> harnx_openapi_tools::SpecResult<Catalog> {
    let document = SpecDocument::from_value(root)?;
    Catalog::build(&document, "srv")
}

fn protected<'a>(catalog: &'a Catalog, name: &str) -> &'a ProtectedParam {
    catalog
        .find_operation("op")
        .unwrap()
        .protected_params
        .iter()
        .find(|param| param.name == name)
        .unwrap_or_else(|| panic!("no protected param {name}"))
}

#[test]
fn reserved_credential_headers_are_protected_not_model_arguments() {
    harnx_core::require_nextest();
    // No apiKey scheme names any of these headers; the document even carries
    // basic auth metadata that says nothing about header parameter names.
    let catalog = build(json!({
        "openapi": "3.0.1",
        "info": {"title": "t", "version": "1"},
        "components": {"securitySchemes": {
            "basicAuth": {"type": "http", "scheme": "basic"}
        }},
        "security": [{"basicAuth": []}],
        "paths": {"/things/{authorization}": {"get": {
            "operationId": "op",
            "parameters": [
                {"name": "Authorization", "in": "header", "required": true,
                 "schema": {"type": "string"}},
                {"name": "proxy-authorization", "in": "header",
                 "schema": {"type": "string"}},
                {"name": "COOKIE", "in": "header", "schema": {"type": "string"}},
                {"name": "X-Request-Id", "in": "header", "schema": {"type": "string"}},
                {"name": "authorization", "in": "query", "schema": {"type": "string"}},
                {"name": "authorization", "in": "path", "required": true,
                 "schema": {"type": "string"}}
            ],
            "responses": {"200": {"description": "ok"}}
        }}}
    }))
    .unwrap();

    for name in ["Authorization", "proxy-authorization", "COOKIE"] {
        let param = protected(&catalog, name);
        assert_eq!(param.scheme, RESERVED_HEADER_SCHEME, "{name}");
        assert_eq!(param.placement, ApiKeyPlacement::Header, "{name}");
    }

    let operation = catalog.find_operation("op").unwrap();
    // The unrelated header stays a model argument; the reserved ones do not.
    let header_names: Vec<&str> = operation
        .header_params
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    assert_eq!(header_names, ["X-Request-Id"]);
    let headers_schema = &operation.input_schema["properties"]["headers"]["properties"];
    assert!(headers_schema.get("Authorization").is_none());
    assert!(headers_schema.get("proxy-authorization").is_none());
    assert!(headers_schema.get("COOKIE").is_none());
    // The required Authorization declaration must not make the remaining
    // optional-only headers group required.
    assert!(!operation.input_schema["required"]
        .as_array()
        .unwrap()
        .contains(&json!("headers")));

    // Same-name query and path arguments are unrelated: they survive.
    assert_eq!(
        operation
            .query_params
            .iter()
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>(),
        ["authorization"]
    );
    assert_eq!(operation.path_params[0].name, "authorization");
}

#[test]
fn swagger2_reserved_credential_headers_are_protected_too() {
    harnx_core::require_nextest();
    let catalog = build(json!({
        "swagger": "2.0",
        "info": {"title": "t", "version": "1"},
        "securityDefinitions": {"basicAuth": {"type": "basic"}},
        "security": [{"basicAuth": []}],
        "paths": {"/a": {"get": {
            "operationId": "op",
            "parameters": [
                {"name": "Authorization", "in": "header", "type": "string", "required": true},
                {"name": "cookie", "in": "header", "type": "string"}
            ],
            "responses": {"200": {"description": "ok"}}
        }}}
    }))
    .unwrap();

    for name in ["Authorization", "cookie"] {
        let param = protected(&catalog, name);
        assert_eq!(param.scheme, RESERVED_HEADER_SCHEME, "{name}");
        assert_eq!(param.placement, ApiKeyPlacement::Header, "{name}");
    }
    // No ordinary headers remain, so no `headers` group is published at all.
    let operation = catalog.find_operation("op").unwrap();
    assert!(operation.header_params.is_empty());
    assert!(operation.input_schema["properties"]
        .get("headers")
        .is_none());
}

#[test]
fn referenced_schemes_resolve_and_classify_through_local_refs() {
    harnx_core::require_nextest();
    // Both schemes are pure Reference Objects whose targets live outside the
    // scanned `components/securitySchemes` declarations; one target is itself
    // reached through a two-hop reference chain.
    let catalog = build(json!({
        "openapi": "3.0.1",
        "info": {"title": "t", "version": "1"},
        "components": {
            "securitySchemes": {
                "outerQuery": {"$ref": "#/components/x/middle"},
                "outerHeader": {"$ref": "#/components/x/hiddenHeader"}
            },
            "x": {
                "middle": {"$ref": "#/components/x/hiddenQuery"},
                "hiddenQuery": {"type": "apiKey", "in": "query", "name": "apiKey"},
                "hiddenHeader": {"type": "apiKey", "in": "header", "name": "X-Token"}
            }
        },
        "security": [{"outerQuery": []}, {"outerHeader": []}],
        "paths": {"/a": {"get": {
            "operationId": "op",
            "parameters": [
                {"name": "apiKey", "in": "query", "schema": {"type": "string"}},
                {"name": "X-Token", "in": "header", "schema": {"type": "string"}},
                {"name": "verbose", "in": "query", "schema": {"type": "boolean"}}
            ],
            "responses": {"200": {"description": "ok"}}
        }}}
    }))
    .unwrap();

    // The reference names classify like inline declarations: the query API
    // key is protected under the referencing scheme, not callable.
    let api_key = protected(&catalog, "apiKey");
    assert_eq!(api_key.scheme, "outerQuery");
    assert_eq!(api_key.placement, ApiKeyPlacement::Query);
    let token = protected(&catalog, "X-Token");
    assert_eq!(token.scheme, "outerHeader");
    assert_eq!(token.placement, ApiKeyPlacement::Header);

    let operation = catalog.find_operation("op").unwrap();
    assert!(operation.header_params.is_empty());
    assert_eq!(
        operation
            .query_params
            .iter()
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>(),
        ["verbose"]
    );

    // The unsupported query placement is diagnosed through the reference with
    // the scheme and parameter names.
    let messages = catalog.credential_diagnostics.join("\n");
    assert!(messages.contains("outerQuery"), "{messages}");
    assert!(messages.contains("query"), "{messages}");
    assert!(messages.contains("apiKey"), "{messages}");
}

#[test]
fn unresolvable_scheme_refs_fail_with_scheme_context() {
    harnx_core::require_nextest();
    let error = build(json!({
        "openapi": "3.0.1",
        "info": {"title": "t", "version": "1"},
        "components": {"securitySchemes": {
            "ghost": {"$ref": "#/components/securitySchemes/nowhere"}
        }},
        "paths": {}
    }))
    .unwrap_err();
    let message = error.to_string();
    assert!(message.contains("ghost"), "{message}");
    assert!(message.contains("nowhere"), "{message}");

    // A ref that resolves to a non-object is rejected too.
    let error = build(json!({
        "openapi": "3.0.1",
        "info": {"title": "t", "version": "1"},
        "components": {"securitySchemes": {"bad": {"$ref": "#/info/title"}}},
        "paths": {}
    }))
    .unwrap_err();
    let message = error.to_string();
    assert!(message.contains("bad"), "{message}");
    assert!(message.contains("scheme object"), "{message}");
}

#[test]
fn external_scheme_refs_fail_with_scheme_context() {
    harnx_core::require_nextest();
    let error = build(json!({
        "openapi": "3.0.1",
        "info": {"title": "t", "version": "1"},
        "components": {"securitySchemes": {
            "ext": {"$ref": "https://example.com/openapi.json#/basic"}
        }},
        "paths": {}
    }))
    .unwrap_err();
    let message = error.to_string();
    assert!(message.contains("ext"), "{message}");
    assert!(
        message.contains("https://example.com/openapi.json"),
        "{message}"
    );
    assert!(message.contains("not supported"), "{message}");
}

#[test]
fn cyclic_scheme_refs_fail_with_scheme_context() {
    harnx_core::require_nextest();
    let error = build(json!({
        "openapi": "3.0.1",
        "info": {"title": "t", "version": "1"},
        "components": {"securitySchemes": {
            "a": {"$ref": "#/components/securitySchemes/b"},
            "b": {"$ref": "#/components/securitySchemes/a"}
        }},
        "paths": {}
    }))
    .unwrap_err();
    let message = error.to_string();
    assert!(message.contains("cyclic"), "{message}");
    assert!(message.contains("a") && message.contains("b"), "{message}");

    // A self-referencing scheme fails the same way.
    let error = build(json!({
        "openapi": "3.0.1",
        "info": {"title": "t", "version": "1"},
        "components": {"securitySchemes": {
            "loop": {"$ref": "#/components/securitySchemes/loop"}
        }},
        "paths": {}
    }))
    .unwrap_err();
    assert!(error.to_string().contains("cyclic"), "{error}");
}
