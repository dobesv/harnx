use serde_json::{Map, Value};

/// Record contradictions without changing schema semantics. `patternProperties`
/// may satisfy a required key, so this diagnostic is limited to literal maps.
pub(super) fn record_contradiction_diagnostic(
    result: &Map<String, Value>,
    context: &str,
    diagnostics: &mut Vec<String>,
) {
    if result.get("additionalProperties") != Some(&Value::Bool(false))
        || result.contains_key("patternProperties")
    {
        return;
    }
    let Some(required) = result.get("required").and_then(Value::as_array) else {
        return;
    };
    let declared = result.get("properties").and_then(Value::as_object);
    let undeclared: Vec<String> = required
        .iter()
        .filter_map(Value::as_str)
        .filter(|name| !declared.is_some_and(|properties| properties.contains_key(*name)))
        .map(str::to_string)
        .collect();
    if undeclared.is_empty() {
        return;
    }
    let names: Vec<String> = undeclared.iter().map(|name| format!("{name:?}")).collect();
    let diagnostic = format!(
        "{context}: schema requires {} but declares no property for it while \
         additionalProperties is false, so no request can satisfy the schema (an \
         upstream/vendor typo); the schema is kept verbatim and the operation \
         cannot construct a valid request pending a plan decision",
        names.join(", ")
    );
    if !diagnostics.contains(&diagnostic) {
        diagnostics.push(diagnostic);
    }
}
