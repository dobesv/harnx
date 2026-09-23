use super::*;

#[test]
fn proxy_schema_adds_an_optional_sandbox_override() {
    let spec = proxy_spec(ToolSpec {
        cancellation_guarantee: Default::default(),
        name: "read".to_string(),
        description: String::new(),
        input_schema: json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"]
        }),
        idempotent_hint: true,
        read_only_hint: true,
        timeout_secs: Some(30),
        meta: None,
    });

    assert_eq!(spec.input_schema["required"], json!(["path"]));
    assert_eq!(
        spec.input_schema["properties"]["sandbox_id"]["type"],
        "string"
    );
    assert_eq!(spec.timeout_secs, Some(0));
}

#[test]
fn proxy_tool_surface_includes_bash_and_filesystem_tools() {
    let bash = harnx_bash_tools::builtin_tool_specs()
        .into_iter()
        .map(proxy_spec)
        .map(|spec| format!("bash_{}", spec.name))
        .collect::<Vec<_>>();
    assert_eq!(
        bash,
        [
            "bash_exec",
            "bash_read_exec_log",
            "bash_spawn",
            "bash_wait",
            "bash_terminate",
            "bash_rollback_file",
        ]
    );

    let fs = harnx_fs_tools::builtin_tool_specs()
        .into_iter()
        .map(proxy_spec)
        .map(|spec| format!("fs_{}", spec.name))
        .collect::<Vec<_>>();
    assert_eq!(
        fs,
        [
            "fs_read",
            "fs_write",
            "fs_edit",
            "fs_insert",
            "fs_re_replace",
            "fs_ls",
            "fs_grep",
            "fs_find",
            "fs_rollback_file",
        ]
    );
}

#[test]
fn endpoint_formats_ipv4_and_ipv6_addresses() {
    // Bash endpoint (port 3002)
    assert_eq!(mcp_endpoint("10.0.0.8", 3002), "http://10.0.0.8:3002/mcp");
    assert_eq!(mcp_endpoint("fd00::8", 3002), "http://[fd00::8]:3002/mcp");

    // FS endpoint (port 3003)
    assert_eq!(mcp_endpoint("10.0.0.8", 3003), "http://10.0.0.8:3003/mcp");
    assert_eq!(mcp_endpoint("fd00::8", 3003), "http://[fd00::8]:3003/mcp");
}

#[test]
fn lifecycle_status_is_explicitly_read_only() {
    let status = lifecycle_specs()
        .into_iter()
        .find(|spec| spec.name == "status")
        .unwrap();
    assert!(status.read_only_hint);
    assert!(status.idempotent_hint);
}
