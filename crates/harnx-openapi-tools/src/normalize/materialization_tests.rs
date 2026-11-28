use super::*;

fn sibling_expansion_document(paths: &[&str]) -> SpecDocument {
    let mut path_items = Map::new();
    for path in paths {
        path_items.insert(
            (*path).to_string(),
            serde_json::json!({
                "post": {
                    "operationId": path.trim_start_matches('/'),
                    "requestBody": {
                        "required": true,
                        "content": {
                            "application/json": {
                                "schema": {
                                    "$ref": "#/components/schemas/Payload",
                                    "minLength": 1
                                }
                            }
                        }
                    },
                    "responses": {"200": {"description": "ok"}}
                }
            }),
        );
    }
    SpecDocument::from_value(serde_json::json!({
        "openapi": "3.0.1",
        "paths": path_items,
        "components": {"schemas": {"Payload": {"type": "string"}}}
    }))
    .unwrap()
}

#[test]
fn retained_sibling_expansion_uses_shared_document_budget_across_operations() {
    harnx_core::require_nextest();
    const VALUE_LIMIT: usize = 1_000;
    const BYTE_LIMIT: usize = 1_000;

    // Identical operations each fit within this budget on their own.
    for path in ["/first", "/second"] {
        let document = sibling_expansion_document(&[path]);
        let schemes = SecuritySchemes::collect(&document).unwrap();
        let mut namer = Namer::new("budget").unwrap();
        let mut diagnostics = Vec::new();
        let (operations, used) = normalize_document_with_budget(
            &document,
            &mut namer,
            &schemes,
            &mut diagnostics,
            MaterializationBudget::new(VALUE_LIMIT, BYTE_LIMIT),
        )
        .unwrap();
        assert_eq!(operations.len(), 1);
        assert!(used.values > 0, "{path} must charge sibling expansion");
        assert!(used.bytes > 0, "{path} must charge estimated bytes");
        assert!(used.values < VALUE_LIMIT);
        assert!(used.bytes < BYTE_LIMIT);
        assert!(used.bytes.saturating_mul(2) > BYTE_LIMIT);
    }

    let document = sibling_expansion_document(&["/first", "/second"]);
    let schemes = SecuritySchemes::collect(&document).unwrap();
    let mut namer = Namer::new("budget").unwrap();
    let mut diagnostics = Vec::new();
    let error = match normalize_document_with_budget(
        &document,
        &mut namer,
        &schemes,
        &mut diagnostics,
        MaterializationBudget::new(VALUE_LIMIT, BYTE_LIMIT),
    ) {
        Ok(_) => panic!("combined sibling expansion should exceed document budget"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("POST /second"), "{error}");
    assert!(
        error.contains("cumulative schema reference materialization budget exceeded"),
        "{error}"
    );
    assert!(error.contains("estimated bytes"), "{error}");
}

#[test]
fn jira_catalog_fits_default_reference_materialization_budget() {
    harnx_core::require_nextest();
    let document =
        SpecDocument::parse(include_bytes!("../../tests/fixtures/jira-swagger-v3.json")).unwrap();
    let schemes = SecuritySchemes::collect(&document).unwrap();
    let mut namer = Namer::new("jira").unwrap();
    let mut diagnostics = Vec::new();
    let (operations, used) = normalize_document_with_budget(
        &document,
        &mut namer,
        &schemes,
        &mut diagnostics,
        MaterializationBudget::new(
            crate::compact::materialization::DEFAULT_MATERIALIZATION_VALUE_BUDGET,
            crate::compact::materialization::DEFAULT_MATERIALIZATION_BYTE_BUDGET,
        ),
    )
    .unwrap();

    assert_eq!(operations.len(), 630);
    assert_eq!(diagnostics.len(), 3);
    assert!(used.values < crate::compact::materialization::DEFAULT_MATERIALIZATION_VALUE_BUDGET);
    assert!(used.bytes < crate::compact::materialization::DEFAULT_MATERIALIZATION_BYTE_BUDGET);
    println!(
        "Jira reference materialization charged {} / {} values and {} / {} estimated bytes",
        used.values,
        crate::compact::materialization::DEFAULT_MATERIALIZATION_VALUE_BUDGET,
        used.bytes,
        crate::compact::materialization::DEFAULT_MATERIALIZATION_BYTE_BUDGET
    );
}
