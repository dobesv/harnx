use super::*;

#[test]
fn mcp_http_args_accept_separate_and_equals_values() {
    let separate = vec![
        "server".to_string(),
        "--host".to_string(),
        "127.0.0.1".to_string(),
        "--port".to_string(),
        "3999".to_string(),
    ];
    assert_eq!(
        mcp_http_arg_value(&separate, "--host").unwrap(),
        Some("127.0.0.1".to_string())
    );
    assert_eq!(
        mcp_http_arg_value(&separate, "--port").unwrap(),
        Some("3999".to_string())
    );

    let equals = vec![
        "server".to_string(),
        "--host=0.0.0.0".to_string(),
        "--port=3001".to_string(),
    ];
    assert_eq!(
        mcp_http_arg_value(&equals, "--host").unwrap(),
        Some("0.0.0.0".to_string())
    );
    assert_eq!(
        mcp_http_arg_value(&equals, "--port").unwrap(),
        Some("3001".to_string())
    );
}

#[test]
fn resolve_mcp_mode_mutual_exclusion() {
    let err = resolve_mcp_mode(["server", "--mcp-stdio", "--mcp-http"]).unwrap_err();
    assert_eq!(
        err.to_string(),
        "choose one of --mcp-stdio or --mcp-http, not both"
    );

    assert_eq!(
        resolve_mcp_mode(["server", "--mcp-stdio"]).unwrap(),
        Some(McpMode::Stdio)
    );
    assert_eq!(
        resolve_mcp_mode(["server", "--mcp-http"]).unwrap(),
        Some(McpMode::Http)
    );
    assert_eq!(resolve_mcp_mode(["server"]).unwrap(), None);
}

#[test]
fn mcp_http_arg_requires_separate_value() {
    let args = vec!["server".to_string(), "--port".to_string()];
    let error = mcp_http_arg_value(&args, "--port").unwrap_err();
    assert_eq!(error.to_string(), "missing value for --port");
}

#[tokio::test]
async fn mcp_factories_receive_no_client_without_reading_nats_configuration() -> Result<()> {
    harnx_core::require_nextest();
    std::env::remove_var("HARNX_SERVER_SCOPE");
    std::env::set_var("HARNX_NATS_URL", "invalid NATS URL");
    std::env::set_var("HARNX_NATS_REPLICAS", "not a number");
    for mode in [McpMode::Stdio, McpMode::Http] {
        let mut calls = 0;
        let ((), transport) = prepare_toolset(Some(mode), |client| {
            calls += 1;
            assert!(client.is_none());
            Ok(())
        })
        .await?;
        assert_eq!(calls, 1);
        assert!(matches!(transport, PreparedTransport::Mcp(actual) if actual == mode));
    }
    Ok(())
}

#[tokio::test]
async fn nats_factory_receives_hosting_connection() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = harnx_test_bins::spawn_nats_server().await? else {
        return Ok(());
    };
    std::env::set_var("HARNX_SERVER_SCOPE", "factory-test");
    std::env::set_var("HARNX_NATS_URL", server.url());
    let mut calls = 0;
    let (client, transport) = prepare_toolset(None, |client| {
        calls += 1;
        Ok(client.expect("NATS factory must receive a client"))
    })
    .await?;
    let PreparedTransport::Nats {
        instance_id,
        connection,
    } = transport
    else {
        panic!("default transport must be NATS");
    };
    assert_eq!(calls, 1);
    assert_eq!(instance_id.as_str(), "factory-test");
    assert_eq!(
        client.server_info().client_id,
        connection.client.server_info().client_id,
        "factory must share hosting connection, not reconnect"
    );
    client.flush().await?;
    connection.client.flush().await?;
    Ok(())
}

#[tokio::test]
async fn factory_error_is_returned_without_starting_server() {
    let result = prepare_toolset::<(), _>(Some(McpMode::Stdio), |_| {
        anyhow::bail!("factory construction failed")
    })
    .await;
    assert_eq!(
        result.err().unwrap().to_string(),
        "factory construction failed"
    );
}

#[test]
fn http_options_preserve_default_limit_unless_overridden() {
    let default = StreamableHttpServerConfig::default().max_request_body_bytes;
    assert_eq!(
        McpHttpOptions::default()
            .apply(StreamableHttpServerConfig::default())
            .max_request_body_bytes,
        default,
    );
    assert_eq!(
        McpHttpOptions {
            max_request_body_bytes: Some(16 * 1024 * 1024),
        }
        .apply(StreamableHttpServerConfig::default())
        .max_request_body_bytes,
        16 * 1024 * 1024,
    );
    assert_eq!(
        StreamableHttpServerConfig::default().max_request_body_bytes,
        default,
    );
}
