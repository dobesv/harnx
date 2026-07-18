use std::fmt::Display;

use jaq_core::load::{Arena, File, Loader};
use jaq_core::{data, unwrap_valr, Compiler, Ctx, Vars};
use jaq_json::Val;
use log::warn;
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

fn run_filter(filter: &JsonFilter, expr: &str, input: Value) -> Option<Value> {
    let input_val = json_to_val(&input);
    let ctx = Ctx::<data::JustLut<Val>>::new(&filter.lut, Vars::new([]));
    let mut outputs = filter.id.run((ctx, input_val)).map(unwrap_valr);

    match outputs.next() {
        Some(Ok(value)) => val_to_json(&value).or(Some(input)),
        Some(Err(err)) => {
            warn!("{}", runtime_failure_message(expr, &err, &input));
            None
        }
        None => Some(input),
    }
}

/// Compiles and runs a single jaq expression against `input`.
/// Returns `None` and logs a warning on parse/compile/runtime error.
pub fn eval_filter(expr: &str, input: Value) -> Option<Value> {
    let filter = match compile_filter(expr) {
        Ok(filter) => filter,
        Err(error) => {
            warn!("jaq parse/compile failed for {expr:?}: {error}");
            return None;
        }
    };

    run_filter(&filter, expr, input)
}

/// Runs expressions in sequence — result of N is input to N+1.
/// On error in expression N: logs warning, skips that expression, continues with unmodified value.
pub fn eval_filters(exprs: &[String], input: Value) -> Value {
    exprs.iter().fold(input, |current, expr| {
        eval_filter(expr, current.clone()).unwrap_or(current)
    })
}

/// Like eval_filters, but returns Err on any expression parse/compile/runtime error.
pub fn eval_filters_strict(exprs: &[String], input: Value) -> anyhow::Result<Value> {
    exprs
        .iter()
        .try_fold(input, |current, expr| eval_filter_strict(expr, current))
}

/// Like eval_filter, but returns Err instead of None on failure.
fn eval_filter_strict(expr: &str, input: Value) -> anyhow::Result<Value> {
    let filter = compile_filter(expr)
        .map_err(|e| anyhow::anyhow!("jaq compile failed for {:?}: {}", expr, e))?;
    // `run_filter` only returns `None` after a jaq runtime error, which it has
    // already logged (via `warn!`) with the failing expression and input kind.
    // A legitimate empty filter yields `Some(input)`, so `None` here always
    // means a runtime failure — point the caller at the logs for the reason.
    run_filter(&filter, expr, input).ok_or_else(|| {
        anyhow::anyhow!("jaq runtime failed for {expr:?}; see logs for the runtime error details")
    })
}

#[cfg(test)]
mod tests {
    use super::{eval_filter, eval_filters, eval_filters_strict, runtime_failure_message};
    use serde_json::{json, Value};

    #[test]
    fn identity_filter_returns_input() {
        let input = json!({"a": 1, "b": [true, false]});
        let output = eval_filter(".", input.clone());
        assert_eq!(output, Some(input));
    }

    #[test]
    fn body_mutation_updates_object() {
        let input = json!({"a": 1});
        let output = eval_filter(".a = 2", input);
        assert_eq!(output, Some(json!({"a": 2})));
    }

    #[test]
    fn invalid_expression_returns_none() {
        let input = json!({"a": 1});
        let output = eval_filter(".a = ", input.clone());
        assert_eq!(output, None);
    }

    #[test]
    fn eval_filters_skips_invalid_expression() {
        let input = json!({"a": 1});
        // invalid expression should be skipped; output = input unchanged
        let chained = eval_filters(&[".a = ".to_string()], input.clone());
        assert_eq!(chained, input);
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
        let output = eval_filter(expr, input);
        assert_eq!(output, Some(json!({"a": 2, "b": 3})));
    }

    #[test]
    fn multi_expression_chain_feeds_next_filter() {
        let input = json!({"a": 1});
        let exprs = vec![".a = 2".to_string(), ".b = .a + 1".to_string()];
        let output = eval_filters(&exprs, input);
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
    fn eval_filters_strict_runtime_error_points_to_logs() {
        // `.[]` on a number is a genuine runtime error (not an empty result).
        // The strict error should name the expression and direct the user to
        // the logs for the underlying reason, rather than claiming "no output".
        let input = json!(1);
        let err = eval_filters_strict(&[".[]".to_string()], input)
            .expect_err("iterating a number must be a runtime error");
        let message = err.to_string();

        assert!(
            message.contains(".[]"),
            "message should name the expr: {message}"
        );
        assert!(
            message.contains("see logs"),
            "message should point to the logs: {message}"
        );
        assert!(
            !message.contains("no output"),
            "message must not claim 'no output' for a runtime error: {message}"
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
