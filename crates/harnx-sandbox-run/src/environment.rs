//! MCP environment tool output parsing and validation.

use std::collections::HashMap;

use anyhow::{Context, Result};
use serde_json::Value;

pub(crate) fn parse_environment_tool_result(
    content: &[rmcp::model::ContentBlock],
) -> Result<HashMap<String, String>> {
    let text = content
        .iter()
        .find_map(|block| block.as_text().map(|t| &t.text))
        .context("MCP tool 'environment' returned no text content block")?;

    let parsed: Value = serde_json::from_str(text)
        .with_context(|| format!("failed to parse environment tool output JSON: {text}"))?;

    let env_obj = parsed
        .get("env")
        .and_then(Value::as_object)
        .context("environment tool output missing 'env' object or 'env' is not an object")?;

    let mut env = HashMap::with_capacity(env_obj.len());
    for (k, v) in env_obj {
        let s = v.as_str().with_context(|| {
            format!("environment variable '{k}' value must be a string, got {v:?}")
        })?;
        env.insert(k.clone(), s.to_string());
    }

    Ok(env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::ContentBlock;

    #[test]
    fn parses_valid_environment_content() {
        let content = vec![ContentBlock::text(
            r#"{"env":{"HTTP_PROXY":"http://127.0.0.1:3009","TEST_VAR":"hello"},"filesystem":{}}"#,
        )];

        let env = parse_environment_tool_result(&content).unwrap();
        assert_eq!(
            env.get("HTTP_PROXY").map(String::as_str),
            Some("http://127.0.0.1:3009")
        );
        assert_eq!(env.get("TEST_VAR").map(String::as_str), Some("hello"));
    }

    #[test]
    fn fails_closed_on_non_string_env_value() {
        let content = vec![ContentBlock::text(
            r#"{"env":{"PORT":8080},"filesystem":{}}"#,
        )];

        assert!(parse_environment_tool_result(&content).is_err());
    }

    #[test]
    fn fails_closed_on_missing_env_key() {
        let content = vec![ContentBlock::text(r#"{"filesystem":{}}"#)];

        assert!(parse_environment_tool_result(&content).is_err());
    }
}
