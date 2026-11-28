use super::*;

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
