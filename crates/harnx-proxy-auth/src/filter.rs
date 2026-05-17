use anyhow::{anyhow, Result};
use jaq_all::{data, fmts, json};
use serde_json::Value;

pub struct CompiledFilter(data::Filter);

pub fn compile(expr: &str) -> Result<CompiledFilter> {
    data::compile(expr)
        .map(CompiledFilter)
        .map_err(|errors| anyhow!("jaq filter compile error: {errors:?}"))
}

pub fn apply_filter(filter: &CompiledFilter, req_json: Value) -> Result<Value> {
    let input = fmts::read::json::parse_single(req_json.to_string().as_bytes())
        .map_err(|error| anyhow!("input conversion: {error}"))?;

    let runner = data::Runner::default();
    let vars = Default::default();
    let mut output = None;

    data::run(
        &runner,
        &filter.0,
        vars,
        std::iter::once(Ok::<_, String>(input)),
        |error| anyhow!(error),
        |value| {
            output = Some(value);
            Ok(())
        },
    )?;

    match output {
        Some(Ok(value)) => jaq_value_to_json(value),
        Some(Err(error)) => Err(anyhow!(error.to_string())),
        None => Ok(req_json),
    }
}

fn jaq_value_to_json(value: json::Val) -> Result<Value> {
    match value {
        json::Val::Null => Ok(Value::Null),
        json::Val::Bool(value) => Ok(Value::Bool(value)),
        json::Val::Num(value) => serde_json::from_str(&value.to_string())
            .map_err(|error| anyhow!("output conversion: {error}")),
        json::Val::BStr(value) | json::Val::TStr(value) => Ok(Value::String(
            String::from_utf8_lossy(value.as_ref()).into_owned(),
        )),
        json::Val::Arr(values) => values
            .iter()
            .cloned()
            .map(jaq_value_to_json)
            .collect::<Result<Vec<_>>>()
            .map(Value::Array),
        json::Val::Obj(map) => map
            .iter()
            .map(|(key, value)| {
                let key = match key {
                    json::Val::BStr(value) | json::Val::TStr(value) => {
                        String::from_utf8_lossy(value.as_ref()).into_owned()
                    }
                    other => other.to_string(),
                };
                Ok((key, jaq_value_to_json(value.clone())?))
            })
            .collect::<Result<serde_json::Map<_, _>>>()
            .map(Value::Object),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{apply_filter, compile};

    fn request_json() -> serde_json::Value {
        json!({
            "method": "GET",
            "host": "github.com",
            "path": "/repos/foo/bar",
            "headers": {
                "content-type": "application/json"
            }
        })
    }

    #[test]
    fn compile_identity_filter() {
        compile(".").expect("identity filter compiles");
    }

    #[test]
    fn compile_and_apply_header_filter() {
        let filter = compile(".headers.authorization = \"Bearer test\"")
            .expect("header assignment compiles");

        let output = apply_filter(&filter, request_json()).expect("filter applies");
        assert_eq!(output["headers"]["authorization"], "Bearer test");
    }

    #[test]
    fn identity_filter_returns_same_object() {
        let filter = compile(".").expect("identity filter compiles");
        let input = request_json();
        let output = apply_filter(&filter, input.clone()).expect("filter applies");
        assert_eq!(output, input);
    }

    #[test]
    fn if_without_else_passes_through_on_no_match() {
        // implicit `else .` — non-matching input returns unchanged
        let filter = compile("if .host == \"evil.com\" then .block = true end")
            .expect("if-without-else compiles");
        let output = apply_filter(&filter, request_json()).expect("filter applies");
        assert_eq!(output, request_json()); // github.com — passes through unchanged
    }

    #[test]
    fn block_true_sets_block_field() {
        let filter = compile("if .host == \"evil.com\" then .block = true else . end")
            .expect("block filter compiles");
        let blocked = apply_filter(&filter, request_json()).expect("filter applies");
        // github.com is not evil.com — no block field
        assert!(blocked.get("block").is_none());

        let evil = serde_json::json!({
            "method": "GET",
            "host": "evil.com",
            "path": "/",
            "headers": {}
        });
        let blocked = apply_filter(&filter, evil).expect("filter applies");
        assert_eq!(blocked["block"], true);
    }

    #[test]
    fn block_string_sets_reason() {
        let filter = compile("if .host == \"evil.com\" then .block = \"not allowed\" else . end")
            .expect("block string filter compiles");
        let evil = serde_json::json!({
            "method": "GET",
            "host": "evil.com",
            "path": "/",
            "headers": {}
        });
        let result = apply_filter(&filter, evil).expect("filter applies");
        assert_eq!(result["block"], "not allowed");
    }

    #[test]
    fn github_auth_filter_uses_env_var() {
        unsafe {
            std::env::set_var("GITHUB_TOKEN", "test-token");
        }

        let filter = compile(
            r#"if .host == "github.com" then .headers.authorization = "Bearer \(env.GITHUB_TOKEN)" else . end"#,
        )
        .expect("github auth filter compiles");

        let output = apply_filter(&filter, request_json()).expect("filter applies");
        assert_eq!(output["headers"]["authorization"], "Bearer test-token");
    }
}
