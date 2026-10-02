use crate::config::apply_client_patch;
use harnx_client::ClientConfig;

fn make_openai_client() -> ClientConfig {
    let mut client: ClientConfig = serde_yaml::from_str("type: openai\napi_key: sk-original\n")
        .expect("should parse openai client config");
    client.set_name("openai".to_string());
    client
}

fn make_claude_client() -> ClientConfig {
    let mut client: ClientConfig = serde_yaml::from_str("type: claude\napi_key: sk-original\n")
        .expect("should parse claude client config");
    client.set_name("claude".to_string());
    client
}

#[test]
fn apply_client_patch_with_identity_expression_leaves_config_unchanged() {
    let mut client = make_openai_client();
    let before = serde_json::to_value(&client).expect("serialize");
    let result = apply_client_patch(&mut client, &[".".to_string()]);
    let after = serde_json::to_value(&client).expect("serialize");
    assert!(result.is_ok());
    assert_eq!(before, after);
}

#[test]
fn apply_client_patch_with_empty_patches_is_noop() {
    let mut client = make_openai_client();
    let before = serde_json::to_value(&client).expect("serialize");
    let result = apply_client_patch(&mut client, &[]);
    let after = serde_json::to_value(&client).expect("serialize");
    assert!(result.is_ok());
    assert_eq!(before, after);
}

#[test]
fn apply_client_patch_sets_field_via_jq_expression() {
    let mut client = make_openai_client();
    let result = apply_client_patch(&mut client, &[r#".api_key = "sk-patched""#.to_string()]);
    assert!(result.is_ok());
    if let ClientConfig::OpenAIConfig(c) = &client {
        assert_eq!(c.api_key.as_deref(), Some("sk-patched"));
    } else {
        panic!("expected OpenAI client, got: {client:?}");
    }
}

#[test]
fn apply_client_patch_name_filter_matches_and_preserves_name() {
    let mut client = make_claude_client();
    client.set_package(Some("pkg".to_string()));

    let result = apply_client_patch(
        &mut client,
        &[r#"if .name == "claude" then .api_key = "patched-key" else . end"#.to_string()],
    );

    assert!(result.is_ok());
    assert_eq!(client.effective_name(), "claude");
    if let ClientConfig::ClaudeConfig(c) = &client {
        assert_eq!(c.api_key.as_deref(), Some("patched-key"));
        assert_eq!(c.package.as_deref(), Some("pkg"));
    } else {
        panic!("expected Claude client, got: {client:?}");
    }
}

#[test]
fn apply_client_patch_with_invalid_jq_expression_returns_err() {
    let mut client = make_openai_client();
    let before = serde_json::to_value(&client).expect("serialize");
    let result = apply_client_patch(&mut client, &[r#".api_key = "unclosed"#.to_string()]);
    let after = serde_json::to_value(&client).expect("serialize");
    assert!(result.is_err());
    assert_eq!(before, after);
}
