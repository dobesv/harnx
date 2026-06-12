/// Test that `ClientConfig` round-trips with `type: "llama-server"`.
#[cfg(unix)]
#[test]
fn test_client_config_roundtrip_llama_server() {
    use crate::ClientConfig;
    use harnx_core::provider_config::llama_server::LlamaServerConfig;

    let config = LlamaServerConfig {
        name: Some("test-llama-server".to_string()),
        model_path: "/path/to/model.gguf".to_string(),
        binary_path: Some("/usr/local/bin/llama-server".to_string()),
        socket_path: Some("/tmp/llama.sock".to_string()),
        ctx_size: Some(2048),
        n_gpu_layers: Some(4),
        threads: Some(2),
        extra_args: Some(vec!["--verbose".to_string()]),
        ..Default::default()
    };

    // Test serialization
    let json = serde_json::to_string(&ClientConfig::LlamaServerConfig(config.clone())).unwrap();
    assert!(
        json.contains(r#""type":"llama-server""#),
        "Serialized JSON should have type: llama-server"
    );

    // Test deserialization from YAML
    let yaml = r#"
type: llama-server
name: test-llama-server
model_path: /path/to/model.gguf
binary_path: /usr/local/bin/llama-server
socket_path: /tmp/llama.sock
ctx_size: 2048
n_gpu_layers: 4
threads: 2
extra_args:
  - --verbose
"#;
    let deserialized: ClientConfig = serde_yaml::from_str(yaml).unwrap();
    match deserialized {
        ClientConfig::LlamaServerConfig(c) => {
            assert_eq!(c.name, Some("test-llama-server".to_string()));
            assert_eq!(c.model_path, "/path/to/model.gguf");
            assert_eq!(c.ctx_size, Some(2048));
        }
        _ => panic!("Expected LlamaServerConfig variant"),
    }
}

/// Test that an unknown provider type deserializes to `ClientConfig::Unknown`.
#[cfg(unix)]
#[test]
fn test_client_config_unknown_type() {
    use crate::ClientConfig;

    let yaml = r#"
type: unknown-provider
name: test
"#;
    let deserialized: ClientConfig = serde_yaml::from_str(yaml).unwrap();
    assert!(matches!(deserialized, ClientConfig::Unknown));
}
