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

fn run_filter(filter: &JsonFilter, input: Value) -> Option<Value> {
    let input_val = json_to_val(&input);
    let ctx = Ctx::<data::JustLut<Val>>::new(&filter.lut, Vars::new([]));
    let mut outputs = filter.id.run((ctx, input_val)).map(unwrap_valr);

    match outputs.next() {
        Some(Ok(value)) => val_to_json(&value).or(Some(input)),
        Some(Err(err)) => {
            warn!("jaq runtime failed: {err}");
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

    run_filter(&filter, input)
}

/// Runs expressions in sequence — result of N is input to N+1.
/// On error in expression N: logs warning, skips that expression, continues with unmodified value.
pub fn eval_filters(exprs: &[String], input: Value) -> Value {
    exprs.iter().fold(input, |current, expr| {
        eval_filter(expr, current.clone()).unwrap_or(current)
    })
}

#[cfg(test)]
mod tests {
    use super::{eval_filter, eval_filters};
    use serde_json::json;

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
}
