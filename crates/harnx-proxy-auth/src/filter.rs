use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use jaq_all::{compile_with, data, defs, fmts, jaq_core::Vars, json};
use jaq_core::native::{bome, run, unary, v};
use jaq_std::ValT as _;
use serde_json::Value;

pub struct CompiledFilter(data::Filter);

const SENTINEL_VAR_NAMES: [&str; 5] = [
    "fake_uuid_key",
    "fake_base64_key",
    "fake_url_base64_key",
    "fake_hex_key",
    "fake_email",
];
const TEMP_FILE_ROOT_VAR_NAME: &str = "temp_file_root";

#[derive(Debug, Clone)]
pub struct JaqVars {
    names: Arc<Vec<String>>,
    values: Arc<Vec<serde_json::Value>>,
}

impl JaqVars {
    pub fn new_from_values(
        sentinels: &crate::sentinel::Sentinels,
        temp_file_root: String,
        loaded: Vec<(String, serde_json::Value)>,
    ) -> Result<Self> {
        let loaded = loaded
            .into_iter()
            .map(|(name, value)| Ok((name, crate::load::serde_json_to_jaq_value(&value)?)))
            .collect::<Result<Vec<_>>>()?;
        Self::new(sentinels, temp_file_root, loaded)
    }

    pub fn new(
        sentinels: &crate::sentinel::Sentinels,
        temp_file_root: String,
        loaded: Vec<(String, json::Val)>,
    ) -> Result<Self> {
        let mut names = Vec::with_capacity(SENTINEL_VAR_NAMES.len() + 1 + loaded.len());
        let mut values = Vec::with_capacity(SENTINEL_VAR_NAMES.len() + 1 + loaded.len());

        for (name, value) in [
            (
                SENTINEL_VAR_NAMES[0].to_owned(),
                serde_json::Value::String(sentinels.uuid_key.clone()),
            ),
            (
                SENTINEL_VAR_NAMES[1].to_owned(),
                serde_json::Value::String(sentinels.base64_key.clone()),
            ),
            (
                SENTINEL_VAR_NAMES[2].to_owned(),
                serde_json::Value::String(sentinels.url_base64_key.clone()),
            ),
            (
                SENTINEL_VAR_NAMES[3].to_owned(),
                serde_json::Value::String(sentinels.hex_key.clone()),
            ),
            (
                SENTINEL_VAR_NAMES[4].to_owned(),
                serde_json::Value::String(sentinels.email.clone()),
            ),
            (
                TEMP_FILE_ROOT_VAR_NAME.to_owned(),
                serde_json::Value::String(temp_file_root),
            ),
        ] {
            names.push(name);
            values.push(value);
        }

        for (name, value) in loaded {
            names.push(name);
            values.push(jaq_value_to_json(value)?);
        }

        Ok(Self {
            names: Arc::new(names),
            values: Arc::new(values),
        })
    }

    pub fn names(&self) -> &[String] {
        self.names.as_ref()
    }

    pub fn temp_file_root(&self) -> Option<&str> {
        self.names
            .iter()
            .position(|name| name == TEMP_FILE_ROOT_VAR_NAME)
            .and_then(|index| self.values.get(index))
            .and_then(Value::as_str)
    }

    pub(crate) fn runtime_vars(&self) -> Result<Vars<json::Val>> {
        let values = self
            .values
            .iter()
            .map(crate::load::serde_json_to_jaq_value)
            .collect::<Result<Vec<_>>>()?;
        Ok(Vars::new(values))
    }
}

pub fn jaq_vars_from_sentinels(sentinels: &crate::sentinel::Sentinels) -> Result<JaqVars> {
    JaqVars::new(sentinels, String::new(), Vec::new())
}

pub fn compile(expr: &str) -> Result<CompiledFilter> {
    let sentinels = crate::sentinel::Sentinels::generate();
    let vars = jaq_vars_from_sentinels(&sentinels)?;
    compile_with_vars(expr, &vars)
}

pub fn compile_with_vars(expr: &str, vars: &JaqVars) -> Result<CompiledFilter> {
    compile_with(expr, defs(), data::funs().chain(auth_funs()), vars.names())
        .map(CompiledFilter)
        .map_err(|errors| anyhow!("jaq filter compile error: {errors:?}"))
}

pub fn apply_filter(
    filter: &CompiledFilter,
    req_json: Value,
    sentinels: &crate::sentinel::Sentinels,
) -> Result<Value> {
    let vars = jaq_vars_from_sentinels(sentinels)?;
    apply_filter_with_vars(filter, req_json, &vars)
}

pub fn apply_filter_with_vars(
    filter: &CompiledFilter,
    req_json: Value,
    vars: &JaqVars,
) -> Result<Value> {
    let input = fmts::read::json::parse_single(req_json.to_string().as_bytes())
        .map_err(|error| anyhow!("input conversion: {error}"))?;

    let runner = data::Runner::default();
    let mut output = None;

    data::run(
        &runner,
        &filter.0,
        vars.runtime_vars()?,
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

/// Compile and run `--env` jaq scripts, injecting jaq variables.
/// Each script receives {} as input and variables as jaq variables.
/// Scripts must output JSON objects; outputs are merged (later keys win).
/// Returns error if any script fails to compile, errors at runtime, outputs non-object, or
/// produces any non-string value (env vars must be strings for MCP tool compatibility).
pub fn eval_env_scripts(
    scripts: &[String],
    vars: &JaqVars,
) -> anyhow::Result<serde_json::Map<String, Value>> {
    let mut merged = serde_json::Map::new();

    for script in scripts {
        let output = run_env_script(script, vars)?;
        merged.extend(output);
    }

    Ok(merged)
}

fn run_env_script(script: &str, vars: &JaqVars) -> Result<serde_json::Map<String, Value>> {
    let filter = compile_with(
        script,
        defs(),
        data::funs().chain(auth_funs()),
        vars.names(),
    )
    .map_err(|errors| anyhow!("jaq env script compile error: {errors:?}"))?;

    let input = fmts::read::json::parse_single(b"{}")
        .map_err(|error| anyhow!("env script input conversion: {error}"))?;
    let runner = data::Runner::default();
    let mut output = None;

    data::run(
        &runner,
        &filter,
        vars.runtime_vars()?,
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
        None => Value::Object(serde_json::Map::new()),
    };

    let Value::Object(map) = value else {
        bail!("jaq env script must output object, got {value}");
    };

    for (key, value) in &map {
        if !value.is_string() {
            bail!("jaq env script output `{key}` must be a string, got {value}");
        }
    }

    Ok(map)
}

pub(crate) fn auth_funs() -> impl Iterator<Item = jaq_core::native::Fun<data::DataKind>> {
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

pub fn jaq_value_to_json(value: json::Val) -> Result<Value> {
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
    use super::{
        apply_filter, apply_filter_with_vars, compile, compile_with_vars, eval_env_scripts, JaqVars,
    };
    use crate::sentinel::Sentinels;
    use serde_json::json;
    use tempfile::tempdir;

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

    fn vars_with_loaded(loaded: Vec<(String, jaq_all::json::Val)>) -> JaqVars {
        JaqVars::new(&test_sentinels(), String::new(), loaded).expect("vars build")
    }

    fn sentinel_vars() -> JaqVars {
        vars_with_loaded(Vec::new())
    }

    #[test]
    fn compile_rejects_invalid_jq() {
        let vars = sentinel_vars();
        let ok = compile_with_vars(".", &vars).expect("identity filter compiles");
        let output =
            apply_filter_with_vars(&ok, request_json(), &vars).expect("identity filter applies");
        assert_eq!(output["host"], "github.com");

        let err = compile("if . then")
            .err()
            .expect("invalid filter should error");
        assert!(err.to_string().contains("compile error"));
    }

    #[test]
    fn compile_and_apply_header_filter() {
        let vars = sentinel_vars();
        let filter = compile_with_vars(".headers.authorization = \"Bearer test\"", &vars)
            .expect("header assignment compiles");

        let output =
            apply_filter_with_vars(&filter, request_json(), &vars).expect("filter applies");
        assert_eq!(output["headers"]["authorization"], "Bearer test");
    }

    #[test]
    fn identity_filter_returns_same_object() {
        let vars = sentinel_vars();
        let filter = compile_with_vars(".", &vars).expect("identity filter compiles");
        let input = request_json();
        let output = apply_filter_with_vars(&filter, input.clone(), &vars).expect("filter applies");
        assert_eq!(output, input);
    }

    #[test]
    fn if_without_else_passes_through_on_no_match() {
        let vars = sentinel_vars();
        let filter = compile_with_vars("if .host == \"evil.com\" then .block = true end", &vars)
            .expect("if-without-else compiles");
        let output =
            apply_filter_with_vars(&filter, request_json(), &vars).expect("filter applies");
        assert_eq!(output, request_json());
    }

    #[test]
    fn block_true_sets_block_field() {
        let vars = sentinel_vars();
        let filter = compile_with_vars(
            "if .host == \"evil.com\" then .block = true else . end",
            &vars,
        )
        .expect("block filter compiles");
        let blocked =
            apply_filter_with_vars(&filter, request_json(), &vars).expect("filter applies");
        assert!(blocked.get("block").is_none());

        let evil = serde_json::json!({
            "method": "GET",
            "host": "evil.com",
            "path": "/",
            "headers": {}
        });
        let blocked = apply_filter_with_vars(&filter, evil, &vars).expect("filter applies");
        assert_eq!(blocked["block"], true);
    }

    #[test]
    fn block_string_sets_reason() {
        let vars = sentinel_vars();
        let filter = compile_with_vars(
            "if .host == \"evil.com\" then .block = \"not allowed\" else . end",
            &vars,
        )
        .expect("block string filter compiles");
        let evil = serde_json::json!({
            "method": "GET",
            "host": "evil.com",
            "path": "/",
            "headers": {}
        });
        let result = apply_filter_with_vars(&filter, evil, &vars).expect("filter applies");
        assert_eq!(result["block"], "not allowed");
    }

    #[test]
    fn github_auth_filter_uses_env_var() {
        unsafe {
            std::env::set_var("GITHUB_TOKEN", "test-token");
        }

        let vars = sentinel_vars();
        let filter = compile_with_vars(
            r#"if .host == "github.com" then .headers.authorization = "Bearer \(env.GITHUB_TOKEN)" else . end"#,
            &vars,
        )
        .expect("github auth filter compiles");

        let output =
            apply_filter_with_vars(&filter, request_json(), &vars).expect("filter applies");
        assert_eq!(output["headers"]["authorization"], "Bearer test-token");
    }

    #[test]
    fn eval_env_scripts_returns_literal_object() {
        let output = eval_env_scripts(&[r#"{"FOO": "bar"}"#.to_owned()], &sentinel_vars())
            .expect("env scripts eval");

        assert_eq!(output.get("FOO"), Some(&json!("bar")));
    }

    #[test]
    fn eval_env_scripts_supports_field_assignment_pattern() {
        let vars = sentinel_vars();
        let output = eval_env_scripts(
            &[r#"if true then .GITHUB_TOKEN = "ghs_test" else . end"#.to_owned()],
            &vars,
        )
        .expect("field assignment pattern should work with {} input");

        assert_eq!(output.get("GITHUB_TOKEN"), Some(&json!("ghs_test")));
    }

    #[test]
    fn eval_env_scripts_injects_sentinel_variables() {
        let sentinels = test_sentinels();
        let vars = JaqVars::new(&sentinels, String::new(), Vec::new()).expect("vars build");
        let output = eval_env_scripts(&[r#"{"K": $fake_uuid_key}"#.to_owned()], &vars)
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
            &sentinel_vars(),
        )
        .expect("env scripts eval");

        assert_eq!(output.get("K"), Some(&json!("second")));
        assert_eq!(output.get("A"), Some(&json!("alpha")));
    }

    #[test]
    fn bearer_formats_authorization_header() {
        let vars = sentinel_vars();
        let output = apply_filter_with_vars(
            &compile_with_vars("bearer(\"abc\")", &vars).expect("bearer compiles"),
            json!(null),
            &vars,
        )
        .expect("bearer applies");

        assert_eq!(output, json!("Bearer abc"));
    }

    #[test]
    fn basic_formats_authorization_header() {
        let vars = sentinel_vars();
        let output = apply_filter_with_vars(
            &compile_with_vars("basic(\"user\"; \"pass\")", &vars).expect("basic compiles"),
            json!(null),
            &vars,
        )
        .expect("basic applies");

        assert_eq!(output, json!("Basic dXNlcjpwYXNz"));
    }

    #[test]
    fn basic_supports_empty_password() {
        let vars = sentinel_vars();
        let output = apply_filter_with_vars(
            &compile_with_vars("basic(\"u\"; \"\")", &vars).expect("basic compiles"),
            json!(null),
            &vars,
        )
        .expect("basic applies");

        assert_eq!(output, json!("Basic dTo="));
    }

    #[test]
    fn hook_filter_interpolates_fake_base64_key() {
        let vars = sentinel_vars();
        let output = apply_filter_with_vars(
            &compile_with_vars(r#""ghs_\($fake_base64_key)""#, &vars).expect("hook compiles"),
            json!(null),
            &vars,
        )
        .expect("hook applies");

        assert_eq!(output, json!("ghs_QUJDREVGR0hJSktMTU5PUA=="));
    }

    #[test]
    fn hook_filter_condition_uses_fake_uuid_key() {
        let vars = sentinel_vars();
        let filter = compile_with_vars(
            r#"if .headers.authorization == "Bearer \($fake_uuid_key)" then .matched = true else . end"#,
            &vars,
        )
        .expect("hook compiles");
        let input = json!({
            "headers": {
                "authorization": "Bearer 11111111-2222-3333-4444-555555555555"
            }
        });

        let output = apply_filter_with_vars(&filter, input, &vars).expect("hook applies");

        assert_eq!(output["matched"], true);
    }

    #[test]
    fn load_yaml_var_resolves_in_hook_expr() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("cfg.yaml");
        std::fs::write(&path, "profiles:\n  - cloud_id: abc123\n").unwrap();
        let loaded = vec![(
            "cfg".to_owned(),
            crate::load::load_value(&crate::load::LoadSpec {
                name: "cfg".into(),
                path,
                format: crate::load::LoadFormat::Yaml,
            }),
        )];
        let vars = vars_with_loaded(loaded);
        let filter = compile_with_vars(r#"$cfg.profiles[0].cloud_id"#, &vars).unwrap();
        let output = apply_filter_with_vars(&filter, json!(null), &vars).unwrap();
        assert_eq!(output, json!("abc123"));
    }

    #[test]
    fn load_json_var_resolves_in_hook_expr() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("cfg.json");
        std::fs::write(&path, "{\"profiles\": [{\"cloud_id\": \"abc123\"}]}\n").unwrap();
        let loaded = vec![(
            "cfg".to_owned(),
            crate::load::load_value(&crate::load::LoadSpec {
                name: "cfg".into(),
                path,
                format: crate::load::LoadFormat::Json,
            }),
        )];
        let vars = vars_with_loaded(loaded);
        let filter = compile_with_vars(r#"$cfg.profiles[0].cloud_id"#, &vars).unwrap();
        let output = apply_filter_with_vars(&filter, json!(null), &vars).unwrap();
        assert_eq!(output, json!("abc123"));
    }

    #[test]
    fn load_raw_var_resolves_in_env_expr() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("cfg.txt");
        std::fs::write(&path, "hello world").unwrap();
        let loaded = vec![(
            "raw_cfg".to_owned(),
            crate::load::load_value(&crate::load::LoadSpec {
                name: "raw_cfg".into(),
                path,
                format: crate::load::LoadFormat::Raw,
            }),
        )];
        let vars = vars_with_loaded(loaded);
        let output = eval_env_scripts(&[r#"{"RAW": $raw_cfg}"#.to_owned()], &vars).unwrap();
        assert_eq!(output.get("RAW"), Some(&json!("hello world")));
    }

    #[test]
    fn missing_loaded_var_is_null() {
        let dir = tempdir().unwrap();
        let loaded = vec![(
            "cfg".to_owned(),
            crate::load::load_value(&crate::load::LoadSpec {
                name: "cfg".into(),
                path: dir.path().join("missing.yaml"),
                format: crate::load::LoadFormat::Yaml,
            }),
        )];
        let vars = vars_with_loaded(loaded);
        let filter = compile_with_vars(r#"if $cfg then "y" else "n" end"#, &vars).unwrap();
        let output = apply_filter_with_vars(&filter, json!(null), &vars).unwrap();
        assert_eq!(output, json!("n"));
    }

    #[test]
    fn loaded_var_reaches_hook_runtime() {
        let vars = vars_with_loaded(vec![("myvar".to_owned(), jaq_all::json::Val::Bool(true))]);
        let filter =
            compile_with_vars(r#"if $myvar then .matched = true else . end"#, &vars).unwrap();
        let output = apply_filter_with_vars(&filter, request_json(), &vars).unwrap();
        assert_eq!(output["matched"], true);
    }

    #[test]
    fn malformed_loaded_var_is_null() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.yaml");
        std::fs::write(&path, "x: [\n").unwrap();
        let loaded = vec![(
            "cfg".to_owned(),
            crate::load::load_value(&crate::load::LoadSpec {
                name: "cfg".into(),
                path,
                format: crate::load::LoadFormat::Yaml,
            }),
        )];
        let vars = vars_with_loaded(loaded);
        let filter = compile_with_vars(r#"if $cfg then "y" else "n" end"#, &vars).unwrap();
        let output = apply_filter_with_vars(&filter, json!(null), &vars).unwrap();
        assert_eq!(output, json!("n"));
    }

    #[test]
    fn temp_file_root_available_in_hook_and_env() {
        let vars = JaqVars::new(
            &test_sentinels(),
            "/tmp/harnx-fs-test".to_owned(),
            Vec::new(),
        )
        .expect("vars build");
        let hook_filter = compile_with_vars(r#"$temp_file_root"#, &vars).unwrap();
        let hook_output = apply_filter_with_vars(&hook_filter, json!(null), &vars).unwrap();
        assert_eq!(hook_output, json!("/tmp/harnx-fs-test"));

        let env_output =
            eval_env_scripts(&[r#"{"ROOT": $temp_file_root}"#.to_owned()], &vars).unwrap();
        assert_eq!(env_output.get("ROOT"), Some(&json!("/tmp/harnx-fs-test")));
    }

    #[test]
    fn eval_env_scripts_errors_on_syntax_error() {
        let err = eval_env_scripts(&["{".to_owned()], &sentinel_vars())
            .expect_err("invalid script should error");

        assert!(err.to_string().contains("compile error"));
    }

    #[test]
    fn eval_env_scripts_errors_on_non_object_output() {
        let err = eval_env_scripts(&["42".to_owned()], &sentinel_vars())
            .expect_err("non-object output should error");

        assert!(err.to_string().contains("must output object"));
    }

    #[test]
    fn eval_env_scripts_errors_on_non_string_value() {
        let err = eval_env_scripts(&[r#"{"PORT": 8080}"#.to_owned()], &sentinel_vars())
            .expect_err("non-string value should error");

        assert!(err.to_string().contains("must be a string"));
    }

    #[test]
    fn eval_env_scripts_empty_list_returns_empty_map() {
        let output = eval_env_scripts(&[], &sentinel_vars()).expect("empty env scripts eval");

        assert!(output.is_empty());
    }

    #[test]
    fn apply_filter_public_api_still_uses_sentinels_only() {
        let sentinels = test_sentinels();
        let filter = compile(r#"$fake_uuid_key"#).expect("compile");
        let output = apply_filter(&filter, json!(null), &sentinels).expect("apply");
        assert_eq!(output, json!(sentinels.uuid_key));
    }
}
