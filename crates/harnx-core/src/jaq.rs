use std::fmt::Display;

use jaq_core::load::{Arena, File, Loader};
use jaq_core::{data, unwrap_valr, Compiler, Ctx, Vars};
use jaq_json::Val;
use serde_json::Value;

type JsonFilter = jaq_core::Filter<data::JustLut<Val>>;

fn compile_filter(expr: &str) -> Result<JsonFilter, String> {
    let arena = Arena::default();
    let defs = jaq_core::defs()
        .chain(jaq_std::defs())
        .chain(jaq_json::defs());
    let loader = Loader::new(defs);
    let modules = loader
        .load(
            &arena,
            File {
                code: expr,
                path: (),
            },
        )
        .map_err(|err| format!("{err:?}"))?;

    let funs = jaq_core::funs::<data::JustLut<Val>>()
        .chain(jaq_std::funs())
        .chain(jaq_json::funs());

    Compiler::<_, data::JustLut<Val>>::default()
        .with_funs(funs)
        .compile(modules)
        .map_err(|err| format!("{err:?}"))
}

/// Convert `serde_json::Value` → `Val` by round-tripping through JSON text.
fn json_to_val(value: &Value) -> Val {
    let json_str = value.to_string();
    jaq_json::read::parse_single(json_str.as_bytes()).unwrap_or_else(|_| Val::Null)
}

/// Convert `Val` → `serde_json::Value` via the `Display` impl (which writes valid JSON).
fn val_to_json(val: &Val) -> Option<Value> {
    let json_str = val.to_string();
    serde_json::from_str(&json_str).ok()
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn runtime_failure_message(expr: &str, err: &impl Display, input: &Value) -> String {
    format!(
        "jaq runtime failed for {expr:?}: {err} (input was {})",
        json_kind(input)
    )
}

fn run_filter(filter: &JsonFilter, expr: &str, input: Value) -> Result<Value, String> {
    let input_val = json_to_val(&input);
    let ctx = Ctx::<data::JustLut<Val>>::new(&filter.lut, Vars::new([]));
    let mut outputs = filter.id.run((ctx, input_val)).map(unwrap_valr);

    match outputs.next() {
        Some(Ok(value)) => Ok(val_to_json(&value).unwrap_or(input)),
        Some(Err(err)) => Err(runtime_failure_message(expr, &err, &input)),
        // A filter such as `empty` legitimately yields nothing; pass the input
        // through rather than treating it as a failure.
        None => Ok(input),
    }
}

/// Compiles and runs a single jaq expression against `input`.
/// Returns the failure message on parse/compile/runtime error.
pub fn eval_filter_checked(expr: &str, input: Value) -> Result<Value, String> {
    let filter = compile_filter(expr)
        .map_err(|error| format!("jaq parse/compile failed for {expr:?}: {error}"))?;

    run_filter(&filter, expr, input)
}

/// Runs expressions in sequence — result of N is input to N+1.
/// Returns Err on any expression parse/compile/runtime error; skipping a failed
/// expression would hand back a value the caller believes was transformed.
pub fn eval_filters_strict(exprs: &[String], input: Value) -> anyhow::Result<Value> {
    exprs.iter().try_fold(input, |current, expr| {
        eval_filter_checked(expr, current).map_err(|message| anyhow::anyhow!(message))
    })
}

#[cfg(test)]
mod tests {
    use super::{eval_filter_checked, eval_filters_strict, runtime_failure_message};
    use serde_json::{json, Value};

    #[test]
    fn identity_filter_returns_input() {
        let input = json!({"a": 1, "b": [true, false]});
        let output = eval_filter_checked(".", input.clone());
        assert_eq!(output, Ok(input));
    }

    #[test]
    fn body_mutation_updates_object() {
        let input = json!({"a": 1});
        let output = eval_filter_checked(".a = 2", input);
        assert_eq!(output, Ok(json!({"a": 2})));
    }

    #[test]
    fn invalid_expression_returns_err() {
        let input = json!({"a": 1});
        assert!(eval_filter_checked(".a = ", input).is_err());
    }

    #[test]
    fn eval_filters_strict_returns_err_on_invalid_expression() {
        let input = json!({"a": 1});
        let result = eval_filters_strict(&[".a = ".to_string()], input);
        assert!(result.is_err());
    }

    #[test]
    fn eval_filters_strict_applies_valid_expressions() {
        let input = json!({"a": 1});
        let result = eval_filters_strict(&[".a = 2".to_string(), ".b = 3".to_string()], input);
        assert_eq!(result.unwrap(), json!({"a": 2, "b": 3}));
    }

    #[test]
    fn multiline_expression_is_treated_as_whitespace() {
        let input = json!({"a": 1});
        let expr = ".a = 2\n| .b = 3";
        let output = eval_filter_checked(expr, input);
        assert_eq!(output, Ok(json!({"a": 2, "b": 3})));
    }

    #[test]
    fn multi_expression_chain_feeds_next_filter() {
        let input = json!({"a": 1});
        let exprs = vec![".a = 2".to_string(), ".b = .a + 1".to_string()];
        let output = eval_filters_strict(&exprs, input).expect("both expressions are valid");
        assert_eq!(output, json!({"a": 2, "b": 3}));
    }

    #[test]
    fn runtime_failure_message_includes_expr_and_input_kind() {
        let expr = ".[]";
        let err = "cannot use null as iterable (array or object)";
        let message = runtime_failure_message(expr, &err, &Value::Null);

        assert!(message.contains("jaq runtime failed for"));
        assert!(message.contains(expr));
        assert!(message.contains("cannot use null as iterable"));
        assert!(message.contains("input was null"));
    }

    #[test]
    fn eval_filters_strict_runtime_error_includes_the_reason() {
        // `.[]` on a number is a genuine runtime error (not an empty result).
        // The strict error must carry the underlying jaq message, since the
        // caller reports it to the user and the debug log may not be enabled.
        let input = json!(1);
        let err = eval_filters_strict(&[".[]".to_string()], input)
            .expect_err("iterating a number must be a runtime error");
        let message = err.to_string();

        assert!(
            message.contains(".[]"),
            "message should name the expr: {message}"
        );
        assert!(
            message.contains("cannot use 1 as iterable"),
            "message should carry the jaq reason: {message}"
        );
        assert!(
            !message.contains("see logs"),
            "message should state the reason instead of deferring to the logs: {message}"
        );
    }

    #[test]
    fn eval_filter_checked_reports_compile_errors() {
        let err = eval_filter_checked(".a = ", json!({"a": 1}))
            .expect_err("a truncated expression must not compile");

        assert!(
            err.contains("jaq parse/compile failed for"),
            "message should say what failed: {err}"
        );
        assert!(err.contains(".a = "), "message should name the expr: {err}");
    }

    #[test]
    fn nested_assignment_needs_an_existing_parent_object() {
        // jaq, unlike jq, does not create missing intermediate objects for a
        // multi-level path assignment. Patches must assign the whole container.
        assert!(eval_filter_checked(".a.b = 1", json!({})).is_err());
        assert_eq!(
            eval_filter_checked(r#".a = {"b": 1}"#, json!({})),
            Ok(json!({"a": {"b": 1}}))
        );
    }

    #[test]
    fn eval_filters_strict_preserves_empty_filter_output() {
        // A filter that legitimately produces no output (e.g. `empty`) must not
        // be treated as an error; strict eval passes the input through unchanged.
        let input = json!({"a": 1});
        let result = eval_filters_strict(&["empty".to_string()], input.clone());
        assert_eq!(result.unwrap(), input);
    }
}
