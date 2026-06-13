/// Test that `ClientConfig` round-trips with `type: "llama-server"`.
#[cfg(unix)]
#[test]
fn test_client_config_roundtrip_llama_server() {
    use crate::ClientConfig;
    use harnx_core::model::ModelData;
    use harnx_core::provider_config::llama_server::LlamaServerConfig;

    // Test config with per-model GGUF path and knobs
    let model = ModelData::new("test-model")
        .with_model_path("/path/to/model.gguf".to_string())
        .with_ctx_size(2048)
        .with_n_gpu_layers(4)
        .with_threads(2)
        .with_extra_args(vec!["--verbose".to_string()])
        .with_socket_path("/tmp/llama.sock".to_string());

    let config = LlamaServerConfig {
        name: "test-llama-server".to_string(),
        models: vec![model],
        binary_path: Some("/usr/local/bin/llama-server".to_string()),
        ..Default::default()
    };

    // Test serialization
    let json = serde_json::to_string(&ClientConfig::LlamaServerConfig(config.clone())).unwrap();
    assert!(
        json.contains(r#""type":"llama-server""#),
        "Serialized JSON should have type: llama-server"
    );
    // Per-model model_path should appear in models array
    assert!(
        json.contains(r#""model_path":"/path/to/model.gguf""#),
        "Serialized JSON should have per-model model_path"
    );

    // Test deserialization from YAML
    let yaml = r#"
type: llama-server
binary_path: /usr/local/bin/llama-server
models:
  - name: test-model
    model_path: /path/to/model.gguf
    ctx_size: 2048
    n_gpu_layers: 4
    threads: 2
    extra_args:
      - --verbose
    socket_path: /tmp/llama.sock
"#;
    let mut deserialized: ClientConfig = serde_yaml::from_str(yaml).unwrap();
    // Set the name as the loader would
    deserialized.set_name("test-llama-server".to_string());
    match deserialized {
        ClientConfig::LlamaServerConfig(c) => {
            assert_eq!(c.name, "test-llama-server");
            assert_eq!(c.models.len(), 1);
            let m = &c.models[0];
            assert_eq!(m.name, "test-model");
            assert_eq!(m.model_path, Some("/path/to/model.gguf".to_string()));
            assert_eq!(m.ctx_size, Some(2048));
            assert_eq!(m.n_gpu_layers, Some(4));
        }
        _ => panic!("Expected LlamaServerConfig variant"),
    }
}

/// Test that hf_repo field serializes/deserializes correctly.
#[cfg(unix)]
#[test]
fn test_client_config_hf_repo() {
    use crate::ClientConfig;
    use harnx_core::model::ModelData;
    use harnx_core::provider_config::llama_server::LlamaServerConfig;

    // Test config with hf_repo instead of model_path
    let model = ModelData::new("hf-model")
        .with_hf_repo("unsloth/gemma-4-E4B-it-GGUF:UD-Q4_K_XL".to_string());

    let config = LlamaServerConfig {
        name: "test-hf".to_string(),
        models: vec![model],
        ..Default::default()
    };

    // Test serialization
    let json = serde_json::to_string(&ClientConfig::LlamaServerConfig(config.clone())).unwrap();
    assert!(
        json.contains(r#""hf_repo":"unsloth/gemma-4-E4B-it-GGUF:UD-Q4_K_XL""#),
        "Serialized JSON should have hf_repo"
    );
    assert!(
        !json.contains("model_path"),
        "Serialized JSON should not have model_path when unset"
    );

    // Test deserialization from YAML
    let yaml = r#"
type: llama-server
models:
  - name: hf-model
    hf_repo: unsloth/gemma-4-E4B-it-GGUF:UD-Q4_K_XL
"#;
    let deserialized: ClientConfig = serde_yaml::from_str(yaml).unwrap();
    match deserialized {
        ClientConfig::LlamaServerConfig(c) => {
            assert_eq!(c.models.len(), 1);
            let m = &c.models[0];
            assert_eq!(m.name, "hf-model");
            assert_eq!(m.hf_repo, Some("unsloth/gemma-4-E4B-it-GGUF:UD-Q4_K_XL".to_string()));
            assert_eq!(m.model_path, None);
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
"#;
    let deserialized: ClientConfig = serde_yaml::from_str(yaml).unwrap();
    assert!(matches!(deserialized, ClientConfig::Unknown));
}
