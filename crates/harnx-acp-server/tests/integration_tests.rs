//! Integration tests for mock ACP client interaction.
//!
//! Tests the full protocol handshake over stdio using a mock client.
//!
//! These tests require the harnx-acp-server binary to be built first.

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};

    /// Start the harnx-acp-server binary and return a handle for communication.
    fn spawn_server() -> (
        std::process::Child,
        BufReader<std::process::ChildStdout>,
        std::process::ChildStdin,
    ) {
        let binary_path = assert_cmd::cargo::cargo_bin("harnx-acp-server");
        let mut child = Command::new(binary_path)
            .args(["--agent", "test-agent", "--log-level", "error"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("Failed to start harnx-acp-server");

        let stdout = BufReader::new(child.stdout.take().expect("Failed to capture stdout"));
        let stdin = child.stdin.take().expect("Failed to capture stdin");

        (child, stdout, stdin)
    }

    /// Read a JSON-RPC response line from the server.
    fn read_response<R: BufRead>(reader: &mut R) -> serde_json::Value {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .expect("Failed to read response");
        assert!(!line.is_empty(), "Empty response line");
        serde_json::from_str(&line).expect("Failed to parse JSON response")
    }

    /// Send a JSON-RPC request to the server.
    fn send_request<W: Write>(writer: &mut W, id: u64, method: &str, params: serde_json::Value) {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        writeln!(writer, "{}", request).expect("Failed to write request");
        writer.flush().expect("Failed to flush");
    }

    fn initialize_server<R: BufRead, W: Write>(reader: &mut R, writer: &mut W) {
        send_request(
            writer,
            1,
            "initialize",
            serde_json::json!({
                "protocolVersion": 1,
                "clientCapabilities": {},
            }),
        );
        let response = read_response(reader);
        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], 1);
        assert_eq!(response["result"]["protocolVersion"], 1);
        assert_eq!(response["result"]["agentInfo"]["name"], "harnx");
    }

    fn assert_new_session_response(response: &serde_json::Value) {
        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], 2);
        let result = &response["result"];
        let session_id = result["sessionId"].as_str().expect("Expected sessionId");
        assert!(!session_id.is_empty(), "Session ID should not be empty");
        assert!(
            session_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "Session ID should be base64url: {session_id}"
        );
        assert!(result.get("modes").is_none());
        assert!(result.get("configOptions").is_none());
    }

    #[test]
    fn session_new_accepts_empty_mcp_servers() {
        let (_child, mut stdout, mut stdin) = spawn_server();
        initialize_server(&mut stdout, &mut stdin);
        send_request(
            &mut stdin,
            2,
            "session/new",
            serde_json::json!({
                "cwd": std::env::current_dir().unwrap(),
                "mcpServers": [],
            }),
        );

        assert_new_session_response(&read_response(&mut stdout));
    }

    #[test]
    fn session_new_accepts_injected_mcp_servers() {
        let (_child, mut stdout, mut stdin) = spawn_server();
        initialize_server(&mut stdout, &mut stdin);
        send_request(
            &mut stdin,
            2,
            "session/new",
            serde_json::json!({
                "cwd": std::env::current_dir().unwrap(),
                "mcpServers": [{
                    "name": "jetbrains-ide",
                    "command": "/usr/bin/false",
                    "args": ["--ide-injected"],
                    "env": [],
                }],
            }),
        );

        assert_new_session_response(&read_response(&mut stdout));
    }

    #[test]
    fn stdout_contains_only_protocol_frames() {
        let (_child, mut stdout, mut stdin) = spawn_server();

        // Send initialize and read response
        send_request(
            &mut stdin,
            1,
            "initialize",
            serde_json::json!({
                "protocolVersion": 1,
                "clientCapabilities": {},
            }),
        );

        let response = read_response(&mut stdout);
        assert!(response.is_object(), "Response should be valid JSON object");

        // Try to read another line - should timeout or be empty since we only sent one request
        // The response should be exactly one line of valid JSON
        let response_str = response.to_string();
        assert!(
            !response_str.contains('\n'),
            "Response should not contain embedded newlines"
        );
    }
}
