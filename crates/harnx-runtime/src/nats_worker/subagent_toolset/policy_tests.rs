use super::*;

#[test]
fn prompt_arguments_reject_removed_budget_and_validate_timeouts() {
    for value in [
        json!(null),
        json!(0),
        json!(-1),
        json!(i64::MIN),
        json!(1),
        json!(600),
        json!(604800),
        json!(2592000),
    ] {
        let parsed = parse_prompt_args(json!({"message": "work", "timeout_secs": value})).unwrap();
        assert_eq!(
            parsed.timeout_secs,
            value.as_u64().filter(|seconds| *seconds > 0)
        );
    }
    assert_eq!(
        parse_prompt_args(json!({"message": "work"}))
            .unwrap()
            .timeout_secs,
        None
    );
    for value in [
        json!(1.5),
        json!(-0.5),
        json!("10"),
        json!("unlimited"),
        json!(true),
        json!(u64::MAX),
        json!(i64::MAX),
    ] {
        assert!(parse_prompt_args(json!({"message": "work", "timeout_secs": value})).is_err());
    }
    for value in [json!(null), json!(0), json!(1)] {
        let mut removed = json!({"message": "work", "token_budget": value});
        harnx_core::tool::JsonSchema::new(session_prompt_spec("helper", None).input_schema)
            .drop_omitted_nulls(&mut removed);
        let error = parse_prompt_args(removed).err().unwrap();
        assert!(error.to_string().contains("unknown field `token_budget`"));
    }
}

#[tokio::test]
async fn generated_prompt_schema_preserves_strict_and_non_strict_null_semantics() {
    for policy in [
        None,
        Some(TargetRunPolicy {
            timeout_secs: 9,
            source: crate::nats_session_metadata::RunLimitsPolicySource::TargetAgent {
                agent_name: "helper".into(),
            },
        }),
        Some(TargetRunPolicy {
            timeout_secs: 86400,
            source: crate::nats_session_metadata::RunLimitsPolicySource::GlobalDefault,
        }),
    ] {
        let spec = session_prompt_spec("helper", policy.as_ref());
        let advice = spec.input_schema["properties"]["timeout_secs"]["description"]
            .as_str()
            .unwrap();
        for expected in [
            "Omit or pass zero unless a specific deadline is needed.",
            "null, zero or negative inherits target policy",
            "86400 seconds / 24 hours",
            "Positive seconds override local allowance",
            "inherited deadlines can shorten",
        ] {
            assert!(advice.contains(expected), "{advice}");
            assert!(spec.description.contains(expected), "{}", spec.description);
        }
        assert!(!advice.contains("token_budget"));
        assert!(!spec.description.contains("token_budget"));
        assert!(!spec.description.contains("retry"));
        assert!(!spec.description.contains("termination"));
        if let Some(policy) = &policy {
            assert!(advice.contains(&format!("{} seconds", policy.timeout_secs)));
        } else {
            assert!(advice.contains("worker resolves it at admission"));
        }
        assert_schema(spec).await;
    }
}

async fn assert_schema(spec: ToolSpec) {
    let canonical =
        harnx_core::tool::JsonSchema::from_tool_schema(spec.input_schema.clone()).unwrap();
    let declaration = harnx_core::tool::ToolDeclaration {
        name: spec.name.clone(),
        description: spec.description.clone(),
        parameters: canonical.clone(),
        mcp_tool_name: None,
        mcp_server_name: None,
        call_template: None,
        result_template: None,
        idempotent_hint: None,
        read_only_hint: None,
        kind: None,
    };
    let data = || harnx_client::client::ChatCompletionsData {
        messages: vec![harnx_core::message::Message::new(
            harnx_core::message::MessageRole::User,
            harnx_core::message::MessageContent::Text("work".into()),
        )],
        temperature: None,
        top_p: None,
        functions: Some(vec![declaration.clone()]),
        stream: false,
        attachments_dir: None,
    };
    let model = harnx_core::model::Model::new("openai", "gpt-4o");
    let non_strict = harnx_client::openai::openai_build_chat_completions_body(data(), &model);
    assert_eq!(
        non_strict["tools"][0]["function"]["parameters"],
        spec.input_schema
    );
    let body = responses_request_body(data()).await;
    let strict = &body["tools"][0];
    assert_eq!(strict["strict"], true);
    assert_eq!(strict["description"], spec.description);
    assert_eq!(
        strict["parameters"]["properties"]["timeout_secs"]["description"],
        spec.input_schema["properties"]["timeout_secs"]["description"]
    );
    assert_eq!(strict["parameters"]["additionalProperties"], false);
    assert_eq!(
        strict["parameters"]["properties"]["timeout_secs"]["type"],
        json!(["integer", "null"])
    );
    assert!(strict["parameters"]["properties"]["timeout_secs"]
        .get("minimum")
        .is_none());
    assert!(strict["parameters"]["properties"]
        .get("token_budget")
        .is_none());
    assert!(strict["parameters"]["properties"]["timeout_secs"]
        .get("default")
        .is_none());
    for schema in [
        canonical,
        harnx_core::tool::JsonSchema::new(strict["parameters"].clone()),
    ] {
        let mut args = json!({"message": "work", "timeout_secs": null, "attachments": null, "session_id": null});
        // Runtime normalization uses canonical schema, including strict-provider nulls.
        harnx_core::tool::JsonSchema::new(spec.input_schema.clone()).drop_omitted_nulls(&mut args);
        assert!(parse_prompt_args(args).is_ok());
        assert!(schema.properties().unwrap().get("token_budget").is_none());
    }
    assert!(parse_prompt_args(json!({"message": null})).is_err());
}

async fn responses_request_body(data: harnx_client::client::ChatCompletionsData) -> Value {
    use harnx_client::Client;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let capture = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let (start, length) = loop {
            let mut chunk = [0; 4096];
            let read = stream.read(&mut chunk).await.unwrap();
            assert!(read > 0);
            request.extend_from_slice(&chunk[..read]);
            if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                let header = String::from_utf8_lossy(&request[..end]).to_lowercase();
                let length: usize = header
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length: "))
                    .unwrap()
                    .trim()
                    .parse()
                    .unwrap();
                break (end + 4, length);
            }
        };
        while request.len() < start + length {
            let mut chunk = [0; 4096];
            let read = stream.read(&mut chunk).await.unwrap();
            assert!(read > 0);
            request.extend_from_slice(&chunk[..read]);
        }
        stream
            .write_all(
                b"HTTP/1.1 400 Bad Request\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
            )
            .await
            .unwrap();
        serde_json::from_slice::<Value>(&request[start..start + length]).unwrap()
    });
    let model = harnx_core::model::Model::from_config(
        "openai",
        &[serde_yaml::from_str("name: gpt-4o\nendpoint: responses\n").unwrap()],
    )
    .remove(0);
    let config = harnx_core::provider_config::openai::OpenAIConfig {
        api_key: Some("test".into()),
        api_base: Some(format!("http://{addr}")),
        ..Default::default()
    };
    let client = harnx_client::OpenAIClient::from_config_for_test(config, model);
    assert!(client
        .chat_completions_inner(&reqwest::Client::new(), data)
        .await
        .is_err());
    capture.await.unwrap()
}
