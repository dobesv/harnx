use std::fmt::Display;

use jaq_core::load::{Arena, File, Loader};
use jaq_core::{data, unwrap_valr, Compiler, Ctx, Vars};
use jaq_json::Val;
use serde_json::Value;

type JsonFilter = jaq_core::Filter<data::JustLut<Val>>;

/// A compiled jaq expression that can be reused across multiple JSON inputs.
pub struct JaqFilter {
    expression: String,
    filter: JsonFilter,
}

impl JaqFilter {
    /// Compile a jaq expression once for repeated evaluation.
    pub fn compile(expression: impl Into<String>) -> Result<Self, String> {
        let expression = expression.into();
        let filter = compile_filter(&expression, &[])
            .map_err(|error| format!("jaq parse/compile failed for {expression:?}: {error}"))?;
        Ok(Self { expression, filter })
    }

    /// Evaluate the compiled expression against one JSON value.
    pub fn evaluate(&self, input: Value) -> Result<Value, String> {
        run_filter(&self.filter, &self.expression, input)
    }
}

fn compile_filter(expr: &str, global_names: &[&str]) -> Result<JsonFilter, String> {
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
        .with_global_vars(global_names.iter().copied())
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
    JaqFilter::compile(expr)?.evaluate(input)
}

/// Runs expressions in sequence — result of N is input to N+1.
/// Returns Err on any expression parse/compile/runtime error; skipping a failed
/// expression would hand back a value the caller believes was transformed.
pub fn eval_filters_strict(exprs: &[String], input: Value) -> anyhow::Result<Value> {
    exprs.iter().try_fold(input, |current, expr| {
        eval_filter_checked(expr, current).map_err(|message| anyhow::anyhow!(message))
    })
}

/// Compile expressions with declared globals, without evaluating them.
///
/// Names include the `$` prefix (for example, `&["$user"]`). Values aren't
/// needed for validation. Errors name the expression and its one-based index.
pub fn validate_filters_with_globals(
    exprs: &[String],
    global_names: &[&str],
) -> anyhow::Result<()> {
    for (index, expr) in exprs.iter().enumerate() {
        compile_filter(expr, global_names).map_err(|error| {
            anyhow::anyhow!(
                "jaq parse/compile failed for filter {} {expr:?}: {error}",
                index + 1
            )
        })?;
    }
    Ok(())
}

/// Apply expressions sequentially with named globals, requiring one JSON output
/// per expression. Names include the `$` prefix (for example, `"$user"`).
///
/// Unlike [`eval_filters_strict`], empty output and multiple outputs are errors.
/// Runtime errors, including an error after the first output, also fail. Errors
/// name the expression and its one-based index. Globals stay fixed across the
/// sequence; each expression receives the previous expression's output as input.
/// An empty expression list returns `input` unchanged.
pub fn eval_filters_single_with_globals(
    exprs: &[String],
    input: Value,
    globals: &[(&str, Value)],
) -> anyhow::Result<Value> {
    let global_names: Vec<_> = globals.iter().map(|(name, _)| *name).collect();
    let global_values: Vec<_> = globals
        .iter()
        .map(|(_, value)| json_to_val(value))
        .collect();
    exprs
        .iter()
        .enumerate()
        .try_fold(input, |current, (index, expr)| {
            let result = compile_filter(expr, &global_names)
                .map_err(|error| format!("jaq parse/compile failed for {expr:?}: {error}"))
                .and_then(|filter| run_filter_single(&filter, expr, current, &global_values));
            result.map_err(|error| anyhow::anyhow!("jaq filter {} failed: {error}", index + 1))
        })
}

fn run_filter_single(
    filter: &JsonFilter,
    expr: &str,
    input: Value,
    global_values: &[Val],
) -> Result<Value, String> {
    let ctx = Ctx::<data::JustLut<Val>>::new(&filter.lut, Vars::new(global_values.iter().cloned()));
    let mut outputs = filter.id.run((ctx, json_to_val(&input))).map(unwrap_valr);
    let value = match outputs.next() {
        Some(Ok(value)) => value,
        Some(Err(err)) => return Err(runtime_failure_message(expr, &err, &input)),
        None => {
            return Err(format!(
                "jaq filter {expr:?} produced no output; expected exactly one"
            ))
        }
    };
    // Looking past the first output catches late runtime errors too. Stop as
    // soon as a second output is found, even for an unbounded output stream.
    match outputs.next() {
        Some(Ok(_)) => Err(format!(
            "jaq filter {expr:?} produced multiple outputs; expected exactly one"
        )),
        Some(Err(err)) => Err(runtime_failure_message(expr, &err, &input)),
        None => val_to_json(&value)
            .ok_or_else(|| format!("jaq filter {expr:?} produced an invalid JSON value")),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        eval_filter_checked, eval_filters_single_with_globals, eval_filters_strict,
        runtime_failure_message, validate_filters_with_globals, JaqFilter,
    };
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
    fn compiled_filter_can_be_reused() {
        let filter = JaqFilter::compile(".value += 1").expect("compile filter");

        assert_eq!(
            filter.evaluate(json!({"value": 1})),
            Ok(json!({"value": 2}))
        );
        assert_eq!(
            filter.evaluate(json!({"value": 4})),
            Ok(json!({"value": 5}))
        );
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
    fn single_output_globals_can_tailor_tools_by_user_group() {
        let exprs = vec![
            r#"if ($user.groups | index("developers")) then . else .use_tools |= map(select(. != "coder" and . != "fs")) end"#.to_string(),
        ];
        validate_filters_with_globals(&exprs, &["$user"]).expect("declared user compiles");
        let input = json!({"use_tools": ["jira", "confluence", "coder", "fs"]});
        for (groups, expected) in [
            (json!(["developers"]), input.clone()),
            (json!([]), json!({"use_tools": ["jira", "confluence"]})),
        ] {
            let output = eval_filters_single_with_globals(
                &exprs,
                input.clone(),
                &[("$user", json!({"groups": groups}))],
            );
            assert_eq!(output.unwrap(), expected);
        }
    }

    #[test]
    fn single_output_globals_keep_names_and_values_in_order() {
        let globals = [
            ("$z", json!({"groups": ["developers"]})),
            ("$a", json!("quotes: \"; $z = null | error(\"injected\")")),
            ("$n", json!(3)),
        ];
        let output = eval_filters_single_with_globals(
            &["[$a, $n, $z.groups]".to_string()],
            Value::Null,
            &globals,
        );
        assert_eq!(output.unwrap(), json!([globals[1].1, 3, ["developers"]]));
    }

    #[test]
    fn single_output_globals_apply_filters_sequentially() {
        let exprs = vec![
            ".groups = $user.groups".to_string(),
            ".count = (.groups | length) | .owner = $user.user_id".to_string(),
        ];
        let output = eval_filters_single_with_globals(
            &exprs,
            json!({}),
            &[(
                "$user",
                json!({"user_id": "owner", "groups": ["developers"]}),
            )],
        );
        assert_eq!(
            output.unwrap(),
            json!({"groups": ["developers"], "count": 1, "owner": "owner"})
        );
    }

    #[test]
    fn globals_validation_does_not_evaluate_filters() {
        let exprs = vec![
            "$user.groups | .[]".to_string(),
            "error(\"runtime only\")".to_string(),
        ];
        validate_filters_with_globals(&exprs, &["$user"]).expect("only compile, don't execute");
    }

    #[test]
    fn globals_reject_undeclared_variables_at_compile_time() {
        let exprs = vec![".".to_string(), "$unknown.groups".to_string()];
        let validation = validate_filters_with_globals(&exprs, &["$user"]).unwrap_err();
        let evaluation = eval_filters_single_with_globals(
            &exprs,
            json!({}),
            &[("$user", json!({"groups": []}))],
        )
        .unwrap_err();
        for error in [validation, evaluation] {
            let message = error.to_string();
            assert!(message.contains("parse/compile failed"), "{message}");
            assert!(message.contains("filter 2"), "{message}");
            assert!(message.contains("$unknown.groups"), "{message}");
        }
        assert!(eval_filter_checked("$user.groups", Value::Null).is_err());
    }

    #[test]
    fn globals_validation_identifies_invalid_filter() {
        let error =
            validate_filters_with_globals(&[".".to_string(), ".a = ".to_string()], &["$user"])
                .unwrap_err()
                .to_string();
        assert!(error.contains("filter 2"), "{error}");
        assert!(error.contains(".a = "), "{error}");
    }

    #[test]
    fn single_output_rejects_empty_output() {
        let error = eval_filters_single_with_globals(
            &[".".to_string(), "empty".to_string()],
            json!({}),
            &[],
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("filter 2"), "{error}");
        assert!(error.contains("empty"), "{error}");
        assert!(error.contains("no output"), "{error}");
    }

    #[test]
    fn single_output_rejects_multiple_and_unbounded_outputs() {
        for expr in ["., .", "range(0; 3)", "repeat(.)"] {
            let error = eval_filters_single_with_globals(
                &[".".to_string(), expr.to_string()],
                json!({}),
                &[],
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains("filter 2"), "{error}");
            assert!(error.contains(expr), "{error}");
            assert!(error.contains("multiple outputs"), "{error}");
        }
    }

    #[test]
    fn single_output_surfaces_runtime_errors_before_and_after_first_output() {
        for expr in ["error(\"denied\")", "., error(\"denied\")"] {
            let error = eval_filters_single_with_globals(
                &[".".to_string(), expr.to_string()],
                json!({}),
                &[],
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains("filter 2"), "{error}");
            assert!(error.contains(&format!("{expr:?}")), "{error}");
            assert!(error.contains("runtime failed"), "{error}");
            assert!(error.contains("denied"), "{error}");
        }
    }

    #[test]
    fn single_output_accepts_null_and_false() {
        for (expr, expected) in [("null", Value::Null), ("false", json!(false))] {
            let output = eval_filters_single_with_globals(&[expr.to_string()], json!({}), &[]);
            assert_eq!(output.unwrap(), expected);
        }
    }

    #[test]
    fn globals_empty_filter_list_is_identity() {
        let input = json!({"use_tools": ["jira"]});
        validate_filters_with_globals(&[], &["$user"]).expect("no filters to compile");
        let output = eval_filters_single_with_globals(&[], input.clone(), &[]);
        assert_eq!(output.unwrap(), input);
    }

    #[test]
    fn eval_filters_strict_keeps_first_of_multiple_outputs() {
        let output = eval_filters_strict(&["., error(\"ignored\")".to_string()], json!(1));
        assert_eq!(output.unwrap(), json!(1));
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
