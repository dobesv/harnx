use async_trait::async_trait;
use harnx_toolset::{ToolInvokeError, ToolSpec, Toolset};
use harnx_toolset_server::{
    compile_enable_globs, toolset_name_from_args, validate_toolset_name, FilteredToolset,
    NamedToolset,
};
use serde_json::{json, Value};
use std::ffi::OsString;
use tokio_util::sync::CancellationToken;

struct FakeToolset;

#[async_trait]
impl Toolset for FakeToolset {
    fn name(&self) -> &str {
        "default"
    }

    fn default_mcp_http_port(&self) -> u16 {
        3456
    }

    fn tools(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            cancellation_guarantee: Default::default(),
            name: "echo".to_string(),
            description: "Echo".to_string(),
            input_schema: json!({"type": "object"}),
            idempotent_hint: false,
            read_only_hint: false,
            timeout_secs: None,
            meta: None,
        }]
    }

    async fn invoke(
        &self,
        tool: &str,
        args: Value,
        _cancel: CancellationToken,
    ) -> Result<Value, ToolInvokeError> {
        Ok(json!({"tool": tool, "args": args}))
    }
}

fn args(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

#[test]
fn absent_name_keeps_default_name() {
    assert_eq!(toolset_name_from_args(&args(&["server"])).unwrap(), None);
    assert_eq!(FakeToolset.name(), "default");
}

#[test]
fn parses_separate_and_inline_name_forms() {
    assert_eq!(
        toolset_name_from_args(&args(&["server", "--name", "review"])).unwrap(),
        Some("review".to_string())
    );
    assert_eq!(
        toolset_name_from_args(&args(&["server", "--name=review_2"])).unwrap(),
        Some("review_2".to_string())
    );
}

#[test]
fn rejects_option_like_name_values_as_missing() {
    for arguments in [
        vec!["server", "--name", "-h"],
        vec!["server", "--name", "-e"],
        vec!["server", "--name=-h"],
    ] {
        let error = toolset_name_from_args(&args(&arguments)).unwrap_err();
        assert!(error.to_string().contains("requires a name argument"));
    }
}

#[test]
fn rejects_missing_empty_duplicate_and_invalid_names() {
    for arguments in [
        vec!["server", "--name"],
        vec!["server", "--name="],
        vec!["server", "--name", "--mcp-stdio"],
        vec!["server", "--name", "-h"],
        vec!["server", "--name", "-e"],
        vec!["server", "--name=-h"],
        vec!["server", "--name=review.tools"],
        vec!["server", "--name=review tools"],
        vec!["server", "--name=rëview"],
        vec!["server", "--name", "one", "--name=two"],
    ] {
        assert!(
            toolset_name_from_args(&args(&arguments)).is_err(),
            "arguments should fail: {arguments:?}"
        );
    }
}

#[test]
fn validation_errors_identify_the_name_source() {
    let error = validate_toolset_name("", "--bash-name/BASH_NAME").unwrap_err();
    assert!(error.to_string().contains("--bash-name/BASH_NAME"));
}

#[tokio::test]
async fn named_toolset_overrides_only_name() {
    let named = NamedToolset::new(FakeToolset, "review".to_string()).unwrap();

    assert_eq!(named.name(), "review");
    assert_eq!(named.default_mcp_http_port(), 3456);
    assert_eq!(named.tools()[0].name, "echo");
    assert_eq!(
        named
            .invoke("echo", json!({"message": "hi"}), CancellationToken::new())
            .await
            .unwrap(),
        json!({"tool": "echo", "args": {"message": "hi"}})
    );
}

#[test]
fn name_override_composes_with_tool_filtering() {
    let named = NamedToolset::new(FakeToolset, "review".to_string()).unwrap();
    let filter = compile_enable_globs(&["echo".to_string()])
        .unwrap()
        .unwrap();
    let filtered = FilteredToolset::new(named, filter);

    assert_eq!(filtered.name(), "review");
    assert_eq!(filtered.default_mcp_http_port(), 3456);
    assert_eq!(filtered.tools().len(), 1);
}
