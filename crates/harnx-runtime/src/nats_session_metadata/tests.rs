use super::store::validate_extensions;
use super::*;
use harnx_core::agent_config::AgentVariables;
use serde_json::Value;
use std::collections::BTreeMap;

fn metadata() -> SessionMetadata {
    SessionMetadata::new(
        "session-1",
        SessionInitializer::named("metis", AgentVariables::default()),
    )
}

#[test]
fn canonical_keys_reserve_future_read_cursor_namespace() {
    assert_eq!(metadata_key("s1"), "sessions/s1/meta");
    assert_eq!(activity_key("s1"), "sessions/s1/activity");
    assert_eq!(read_cursor_key("s1", "alice"), "sessions/s1/read/alice");
}

#[test]
fn metadata_round_trip_preserves_identity_and_private_values() {
    let mut value = metadata();
    value.variables.insert("TOKEN".into(), "secret".into());
    value
        .extensions
        .insert("example".into(), serde_json::json!({"x": 1}));
    let encoded = serde_json::to_vec(&value).unwrap();
    let decoded: SessionMetadata = serde_json::from_slice(&encoded).unwrap();
    decoded.validate("session-1").unwrap();
    assert_eq!(decoded, value);
}

#[test]
fn metadata_rejects_identity_and_extension_size_violations() {
    let value = metadata();
    assert!(value.validate("different").is_err());

    let mut extensions = BTreeMap::new();
    extensions.insert(
        "large".into(),
        Value::String("x".repeat(EXTENSION_NAMESPACE_MAX_BYTES + 1)),
    );
    assert!(validate_extensions(&extensions).is_err());
}

#[test]
fn redacted_view_hides_inline_instructions_and_variable_values() {
    let mut value = SessionMetadata::new(
        "private-session",
        SessionInitializer::inline(
            "private inline instructions",
            AgentVariables::from_iter([("TOKEN".to_string(), "secret-value".to_string())]),
            SessionOverrides::default(),
        ),
    );
    value.title.value = Some("Public title".to_string());
    let redacted = RedactedSessionMetadata::new(
        MetadataRecord {
            metadata: value,
            revision: 11,
        },
        None,
    );
    let json = serde_json::to_string(&redacted).unwrap();
    assert!(!json.contains("private inline instructions"));
    assert!(!json.contains("secret-value"));
    assert!(json.contains("TOKEN"));
    assert!(json.contains("Public title"));
    assert!(json.contains("\"revision\":11"));
}

#[test]
fn typed_patch_rejects_identity_mutations() {
    let error = serde_json::from_value::<SessionMetadataPatch>(serde_json::json!({
        "session_id": "different"
    }))
    .expect_err("identity fields are not patchable");
    assert!(error.to_string().contains("unknown field"));
}

#[test]
fn initializer_uses_remote_identity_and_cli_variables() {
    let variables = AgentVariables::from_iter([("TOKEN".to_string(), "secret".to_string())]);
    let config = crate::config::Config {
        remote_agent: Some(("atlas".to_string(), "production".to_string())),
        agent_variables: Some(variables.clone()),
        ..Default::default()
    };

    let initializer = SessionInitializer::from_config(&config).unwrap();
    assert_eq!(
        initializer,
        SessionInitializer::named("atlas", variables.clone())
    );
    assert_eq!(
        SessionInitializer::named_from_config("atlas", &config),
        SessionInitializer::named("atlas", variables)
    );
}

#[test]
fn initializer_mismatch_error_redacts_inline_instructions() {
    let metadata = SessionMetadata::new(
        "private-session",
        SessionInitializer::inline(
            "stored private instructions",
            AgentVariables::default(),
            SessionOverrides::default(),
        ),
    );
    let requested = SessionInitializer::inline(
        "requested private instructions",
        AgentVariables::default(),
        SessionOverrides::default(),
    );

    let error = metadata
        .validate_initializer(&requested)
        .expect_err("different inline instructions must not match");
    let message = error.to_string();

    assert!(message.contains("existing inline agent, requested inline agent"));
    assert!(!message.contains("stored private instructions"));
    assert!(!message.contains("requested private instructions"));
}
