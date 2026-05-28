use anyhow::{anyhow, bail, Result};
use jaq_all::{compile_with, data, defs, fmts, jaq_core::Vars, json};
use jaq_core::native::{bome, run, unary, v};
use jaq_std::ValT as _;
use serde_json::Value;

pub struct CompiledFilter(data::Filter);

const ENV_SCRIPT_VAR_NAMES: [&str; 5] = [
    "fake_uuid_key",
    "fake_base64_key",
    "fake_url_base64_key",
    "fake_hex_key",
    "fake_email",
];

pub fn compile(expr: &str) -> Result<CompiledFilter> {
    let var_names = ENV_SCRIPT_VAR_NAMES.map(str::to_owned);
    compile_with(expr, defs(), data::funs().chain(auth_funs()), &var_names)
        .map(CompiledFilter)
        .map_err(|errors| anyhow!("jaq filter compile error: {errors:?}"))
}

pub fn apply_filter(
    filter: &CompiledFilter,
    req_json: Value,
    sentinels: &crate::sentinel::Sentinels,
) -> Result<Value> {
    let input = fmts::read::json::parse_single(req_json.to_string().as_bytes())
        .map_err(|error| anyhow!("input conversion: {error}"))?;

    let runner = data::Runner::default();
    let vars = Vars::new([
        sentinel_string_to_jaq_value(&sentinels.uuid_key)?,
        sentinel_string_to_jaq_value(&sentinels.base64_key)?,
        sentinel_string_to_jaq_value(&sentinels.url_base64_key)?,
        sentinel_string_to_jaq_value(&sentinels.hex_key)?,
        sentinel_string_to_jaq_value(&sentinels.email)?,
    ]);
    let mut output = None;

    data::run(
        &runner,
        &filter.0,
        vars,
        std::iter::once(Ok::<_, String>(input)),
        |value| Ok::<_, String>(Some(value)),
        |result| {
            output = Some(result);
            Ok(())
        },
    )
    .map_err(|error| anyhow!("jaq filter execution failed: {error:?}"))?;

    match output {
        Some(Ok(value)) => jaq_value_to_json(value),
        Some(Err(error)) => bail!("jaq filter runtime error: {error}"),
        None => Ok(req_json),
    }
}

/// Compile and run `--env` jaq scripts, injecting sentinel variables.
/// Each script receives {} as input and sentinel values as jaq variables.
/// Scripts must output JSON objects; outputs are merged (later keys win).
/// Returns error if any script fails to compile, errors at runtime, outputs non-object, or
/// produces any non-string value (env vars must be strings for MCP tool compatibility).
pub fn eval_env_scripts(
    scripts: &[String],
    sentinels: &crate::sentinel::Sentinels,
) -> anyhow::Result<serde_json::Map<String, Value>> {
    let mut merged = serde_json::Map::new();

    for script in scripts {
        let output = run_env_script(script, sentinels)?;
        merged.extend(output);
    }

    Ok(merged)
}

fn run_env_script(
    script: &str,
    sentinels: &crate::sentinel::Sentinels,
) -> Result<serde_json::Map<String, Value>> {
    let var_names = ENV_SCRIPT_VAR_NAMES.map(str::to_owned);
    let filter = compile_with(script, defs(), data::funs().chain(auth_funs()), &var_names)
        .map_err(|errors| anyhow!("jaq env script compile error: {errors:?}"))?;

    let vars = Vars::new([
        sentinel_string_to_jaq_value(&sentinels.uuid_key)?,
        sentinel_string_to_jaq_value(&sentinels.base64_key)?,
        sentinel_string_to_jaq_value(&sentinels.url_base64_key)?,
        sentinel_string_to_jaq_value(&sentinels.hex_key)?,
        sentinel_string_to_jaq_value(&sentinels.email)?,
    ]);
    let input = fmts::read::json::parse_single(b"{}")
        .map_err(|error| anyhow!("env script input conversion: {error}"))?;
    let runner = data::Runner::default();
    let mut output = None;

    data::run(
        &runner,
        &filter,
        vars,
        std::iter::once(Ok::<_, String>(input)),
        |value| Ok::<_, String>(Some(value)),
        |result| {
            output = Some(result);
            Ok(())
        },
    )
    .map_err(|error| anyhow!("jaq env script execution failed: {error:?}"))?;

    let value = match output {
        Some(Ok(value)) => jaq_value_to_json(value)?,
        Some(Err(error)) => bail!("jaq env script runtime error: {error}"),
        None => Value::Null,
    };
    let object = value
        .as_object()
        .cloned()
        .ok_or_else(|| anyhow!("jaq env script must output object, got {value}"))?;

    for (key, val) in &object {
        if !val.is_string() {
            bail!("jaq env script output value for key {key:?} must be a string, got {val}");
        }
    }

    Ok(object)
}

fn auth_funs() -> impl Iterator<Item = jaq_core::native::Fun<data::DataKind>> {
    use base64::Engine as _;

    fn bearer(cv: jaq_core::Cv<'_, data::DataKind>) -> jaq_core::ValXs<'_, json::Val> {
        unary(cv, |_input, token| {
            let token = token.try_as_bytes()?;
            Ok(json::Val::from_utf8_bytes(
                format!("Bearer {}", String::from_utf8_lossy(token)).into_bytes(),
            ))
        })
    }

    fn basic(mut cv: jaq_core::Cv<'_, data::DataKind>) -> jaq_core::ValXs<'_, json::Val> {
        let pass = cv.0.pop_var();
        let user = cv.0.pop_var();
        bome((|| {
            let user = user.try_as_bytes()?;
            let pass = pass.try_as_bytes()?;
            let encoded =
                base64::engine::general_purpose::STANDARD.encode([user, b":", pass].concat());
            Ok(json::Val::from_utf8_bytes(
                format!("Basic {encoded}").into_bytes(),
            ))
        })())
    }

    [
        run::<data::DataKind>(("bearer", v(1), bearer)),
        run::<data::DataKind>(("basic", v(2), basic)),
    ]
    .into_iter()
}

fn sentinel_string_to_jaq_value(value: &str) -> Result<json::Val> {
    fmts::read::json::parse_single(format!("{value:?}").as_bytes())
        .map_err(|error| anyhow!("sentinel conversion: {error}"))
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
    use super::{apply_filter, compile, eval_env_scripts};
    use crate::sentinel::Sentinels;
    use serde_json::json;

    fn request_json() -> serde_json::Value {
        json!({
            "method": "GET",
            "host": "github.com",
            "path": "/login",
            "headers": {
                "user-agent": "curl/8.0"
            }
        })
    }

    fn test_sentinels() -> Sentinels {
        Sentinels {
            uuid_key: "11111111-2222-3333-4444-555555555555".to_owned(),
            base64_key: "QUJDREVGR0hJSktMTU5PUA==".to_owned(),
            url_base64_key: "QUJDREVGR0hJSktMTU5PUA".to_owned(),
            hex_key: "00112233445566778899aabbccddeeff".to_owned(),
            email: "fake@example.com".to_owned(),
        }
    }

    #[test]
    fn compile_rejects_invalid_jq() {
        let err = compile(".").expect("identity filter compiles");
        let output =
            apply_filter(&err, request_json(), &test_sentinels()).expect("identity filter applies");
        assert_eq!(output["host"], "github.com");

        let err = compile("if . then")
            .err()
            .expect("invalid filter should error");
        assert!(err.to_string().contains("compile error"));
    }

    #[test]
    fn compile_and_apply_header_filter() {
        let filter = compile(".headers.authorization = \"Bearer test\"")
            .expect("header assignment compiles");

        let output =
            apply_filter(&filter, request_json(), &test_sentinels()).expect("filter applies");
        assert_eq!(output["headers"]["authorization"], "Bearer test");
    }

    #[test]
    fn identity_filter_returns_same_object() {
        let filter = compile(".").expect("identity filter compiles");
        let input = request_json();
        let output =
            apply_filter(&filter, input.clone(), &test_sentinels()).expect("filter applies");
        assert_eq!(output, input);
    }

    #[test]
    fn if_without_else_passes_through_on_no_match() {
        // implicit `else .` — non-matching input returns unchanged
        let filter = compile("if .host == \"evil.com\" then .block = true end")
            .expect("if-without-else compiles");
        let output =
            apply_filter(&filter, request_json(), &test_sentinels()).expect("filter applies");
        assert_eq!(output, request_json()); // github.com — passes through unchanged
    }

    #[test]
    fn block_true_sets_block_field() {
        let filter = compile("if .host == \"evil.com\" then .block = true else . end")
            .expect("block filter compiles");
        let blocked =
            apply_filter(&filter, request_json(), &test_sentinels()).expect("filter applies");
        // github.com is not evil.com — no block field
        assert!(blocked.get("block").is_none());

        let evil = serde_json::json!({
            "method": "GET",
            "host": "evil.com",
            "path": "/",
            "headers": {}
        });
        let blocked = apply_filter(&filter, evil, &test_sentinels()).expect("filter applies");
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
        let result = apply_filter(&filter, evil, &test_sentinels()).expect("filter applies");
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

        let output =
            apply_filter(&filter, request_json(), &test_sentinels()).expect("filter applies");
        assert_eq!(output["headers"]["authorization"], "Bearer test-token");
    }

    #[test]
    fn eval_env_scripts_returns_literal_object() {
        let output = eval_env_scripts(&[r#"{"FOO": "bar"}"#.to_owned()], &test_sentinels())
            .expect("env scripts eval");

        assert_eq!(output.get("FOO"), Some(&json!("bar")));
    }

    #[test]
    fn eval_env_scripts_supports_field_assignment_pattern() {
        // The primary intended usage: .REAL_VAR = "sentinel_value"
        // This only works if the script input is {} (object), not null.
        let sentinels = test_sentinels();
        let output = eval_env_scripts(
            &[r#"if true then .GITHUB_TOKEN = "ghs_test" else . end"#.to_owned()],
            &sentinels,
        )
        .expect("field assignment pattern should work with {} input");

        assert_eq!(output.get("GITHUB_TOKEN"), Some(&json!("ghs_test")));
    }

    #[test]
    fn eval_env_scripts_injects_sentinel_variables() {
        let sentinels = test_sentinels();
        let output = eval_env_scripts(&[r#"{"K": $fake_uuid_key}"#.to_owned()], &sentinels)
            .expect("env scripts eval");

        assert_eq!(output.get("K"), Some(&json!(sentinels.uuid_key)));
    }

    #[test]
    fn eval_env_scripts_later_scripts_overwrite_keys() {
        let output = eval_env_scripts(
            &[
                r#"{"K": "first", "A": "alpha"}"#.to_owned(),
                r#"{"K": "second"}"#.to_owned(),
            ],
            &test_sentinels(),
        )
        .expect("env scripts eval");

        assert_eq!(output.get("K"), Some(&json!("second")));
        assert_eq!(output.get("A"), Some(&json!("alpha")));
    }

    #[test]
    fn bearer_formats_authorization_header() {
        let output = apply_filter(
            &compile("bearer(\"abc\")").expect("bearer compiles"),
            json!(null),
            &test_sentinels(),
        )
        .expect("bearer applies");

        assert_eq!(output, json!("Bearer abc"));
    }

    #[test]
    fn basic_formats_authorization_header() {
        let output = apply_filter(
            &compile("basic(\"user\"; \"pass\")").expect("basic compiles"),
            json!(null),
            &test_sentinels(),
        )
        .expect("basic applies");

        assert_eq!(output, json!("Basic dXNlcjpwYXNz"));
    }

    #[test]
    fn basic_supports_empty_password() {
        let output = apply_filter(
            &compile("basic(\"u\"; \"\")").expect("basic compiles"),
            json!(null),
            &test_sentinels(),
        )
        .expect("basic applies");

        assert_eq!(output, json!("Basic dTo="));
    }

    #[test]
    fn hook_filter_interpolates_fake_base64_key() {
        let output = apply_filter(
            &compile(r#""ghs_\($fake_base64_key)""#).expect("hook compiles"),
            json!(null),
            &test_sentinels(),
        )
        .expect("hook applies");

        assert_eq!(output, json!("ghs_QUJDREVGR0hJSktMTU5PUA=="));
    }

    #[test]
    fn hook_filter_condition_uses_fake_uuid_key() {
        let filter = compile(
            r#"if .headers.authorization == "Bearer \($fake_uuid_key)" then .matched = true else . end"#,
        )
        .expect("hook compiles");
        let input = json!({
            "headers": {
                "authorization": "Bearer 11111111-2222-3333-4444-555555555555"
            }
        });

        let output = apply_filter(&filter, input, &test_sentinels()).expect("hook applies");

        assert_eq!(output["matched"], true);
    }

    #[test]
    fn eval_env_scripts_errors_on_syntax_error() {
        let err = eval_env_scripts(&["{".to_owned()], &test_sentinels())
            .expect_err("invalid script should error");

        assert!(err.to_string().contains("compile error"));
    }

    #[test]
    fn eval_env_scripts_errors_on_non_object_output() {
        let err = eval_env_scripts(&["42".to_owned()], &test_sentinels())
            .expect_err("non-object output should error");

        assert!(err.to_string().contains("must output object"));
    }

    #[test]
    fn eval_env_scripts_errors_on_non_string_value() {
        let err = eval_env_scripts(&[r#"{"PORT": 8080}"#.to_owned()], &test_sentinels())
            .expect_err("non-string value should error");

        assert!(err.to_string().contains("must be a string"));
    }

    #[test]
    fn eval_env_scripts_empty_list_returns_empty_map() {
        let output = eval_env_scripts(&[], &test_sentinels()).expect("empty env scripts eval");

        assert!(output.is_empty());
    }
}
