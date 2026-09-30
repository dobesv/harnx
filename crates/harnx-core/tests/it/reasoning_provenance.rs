use harnx_core::tool::{ReasoningProtocol, ReasoningProvenance, ToolCall};
use serde_json::json;

fn provenance(protocol: ReasoningProtocol, model: &str) -> Option<ReasoningProvenance> {
    Some(ReasoningProvenance {
        protocol,
        model: Some(model.to_string()),
    })
}

fn signed_call(protocol: ReasoningProtocol, model: &str, signature: &str) -> ToolCall {
    ToolCall::new(
        "test".to_string(),
        json!({}),
        Some("id".to_string()),
        Some(signature.to_string()),
    )
    .with_provenance(provenance(protocol, model))
}

fn assert_protocol_serde_name(protocol: ReasoningProtocol, wire_name: &str) {
    let serialized = serde_json::to_string(&protocol).unwrap();
    assert_eq!(serialized, format!("\"{wire_name}\""));
    assert_eq!(
        serde_json::from_str::<ReasoningProtocol>(&serialized).unwrap(),
        protocol
    );
}

fn assert_replays_only_to(
    call: &ToolCall,
    expected_protocol: ReasoningProtocol,
    destination_model: &str,
    signature: &str,
) {
    for protocol in [
        ReasoningProtocol::AnthropicThinking,
        ReasoningProtocol::GeminiThoughtSignature,
        ReasoningProtocol::OpenAiEncryptedReasoning,
    ] {
        let expected = (protocol == expected_protocol).then_some(signature);
        assert_eq!(
            call.compatible_signature(protocol, destination_model),
            expected,
            "stored {expected_protocol:?}, destination {protocol:?}"
        );
    }
}

#[test]
fn reasoning_protocol_serde_names_are_stable() {
    assert_protocol_serde_name(ReasoningProtocol::AnthropicThinking, "anthropic_thinking");
    assert_protocol_serde_name(
        ReasoningProtocol::GeminiThoughtSignature,
        "gemini_thought_signature",
    );
    assert_protocol_serde_name(
        ReasoningProtocol::OpenAiEncryptedReasoning,
        "openai_encrypted_reasoning",
    );
}

#[test]
fn absent_optional_provenance_fields_are_omitted_or_defaulted() {
    let value = ReasoningProvenance {
        protocol: ReasoningProtocol::AnthropicThinking,
        model: None,
    };
    assert_eq!(
        serde_json::to_string(&value).unwrap(),
        r#"{"protocol":"anthropic_thinking"}"#
    );

    let call = ToolCall::new("tool".to_string(), json!({}), None, None);
    assert!(!serde_json::to_string(&call)
        .unwrap()
        .contains("reasoning_provenance"));
    let decoded: ReasoningProvenance =
        serde_json::from_str(r#"{"protocol":"gemini_thought_signature"}"#).unwrap();
    assert_eq!(decoded.model, None);
}

#[test]
fn non_model_bound_protocols_replay_only_to_matching_protocols() {
    for (protocol, source_model, destination_model) in [
        (
            ReasoningProtocol::AnthropicThinking,
            "claude-sonnet",
            "claude-opus",
        ),
        (
            ReasoningProtocol::GeminiThoughtSignature,
            "gemini-flash",
            "gemini-pro",
        ),
    ] {
        let call = signed_call(protocol, source_model, "signature");
        assert_replays_only_to(&call, protocol, destination_model, "signature");
    }
}

#[test]
fn openai_replay_is_model_bound_and_fail_closed() {
    let call = signed_call(
        ReasoningProtocol::OpenAiEncryptedReasoning,
        "gpt-5.6-terra",
        "encrypted",
    );
    assert_replays_only_to(
        &call,
        ReasoningProtocol::OpenAiEncryptedReasoning,
        "gpt-5.6-terra",
        "encrypted",
    );
    assert_eq!(
        call.compatible_signature(ReasoningProtocol::OpenAiEncryptedReasoning, "gpt-5.6-sol"),
        None
    );
}

#[test]
fn unknown_or_absent_signatures_are_incompatible() {
    let legacy = ToolCall::new(
        "test".to_string(),
        json!({}),
        Some("id".to_string()),
        Some("legacy".to_string()),
    );
    let unsigned = ToolCall::new("test".to_string(), json!({}), None, None);
    for protocol in [
        ReasoningProtocol::AnthropicThinking,
        ReasoningProtocol::GeminiThoughtSignature,
        ReasoningProtocol::OpenAiEncryptedReasoning,
    ] {
        assert_eq!(legacy.compatible_signature(protocol, "model"), None);
        assert_eq!(unsigned.compatible_signature(protocol, "model"), None);
    }
}

#[test]
fn legacy_tool_call_deserializes_without_provenance() {
    let yaml =
        "name: test_tool\narguments:\n  foo: bar\nid: tool_123\nthought_signature: legacy_sig\n";
    let call: ToolCall = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(call.name, "test_tool");
    assert_eq!(call.thought_signature.as_deref(), Some("legacy_sig"));
    assert_eq!(call.reasoning_provenance, None);
    assert_eq!(
        call.compatible_signature(ReasoningProtocol::AnthropicThinking, "claude"),
        None
    );
}

#[test]
fn tool_call_roundtrip_preserves_provenance() {
    let call = signed_call(
        ReasoningProtocol::GeminiThoughtSignature,
        "gemini-pro",
        "signature",
    );
    let yaml = serde_yaml::to_string(&call).unwrap();
    let restored: ToolCall = serde_yaml::from_str(&yaml).unwrap();
    assert_eq!(restored.thought_signature.as_deref(), Some("signature"));
    assert_eq!(
        restored.reasoning_provenance,
        provenance(ReasoningProtocol::GeminiThoughtSignature, "gemini-pro")
    );
}
