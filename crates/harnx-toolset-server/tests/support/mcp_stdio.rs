//! Shared real-process MCP stdio fixture for native environment tool services.
use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout, Command},
};

pub struct Session {
    child: Child,
    input: ChildStdin,
    output: Lines<BufReader<ChildStdout>>,
}

impl Session {
    pub async fn start(command: &mut Command, name: &str, filtered: bool) -> Result<Self> {
        let mut child = command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()?;
        let input = child.stdin.take().context("stdin")?;
        let output = BufReader::new(child.stdout.take().context("stdout")?).lines();
        let mut session = Self {
            child,
            input,
            output,
        };
        let init = session.request(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"environment-test","version":"1"},
        }})).await?;
        assert_eq!(
            init["result"]["serverInfo"]["name"],
            format!("harnx-{name}-server")
        );
        session
            .input
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
            .await?;
        let tools = session
            .request(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
            .await?;
        let specs = tools["result"]["tools"].as_array().context("tool list")?;
        assert_eq!(specs.len(), usize::from(!filtered));
        if !filtered {
            assert_eq!(specs[0]["name"], "environment");
        }
        Ok(session)
    }

    async fn request(&mut self, request: Value) -> Result<Value> {
        self.input
            .write_all(format!("{request}\n").as_bytes())
            .await?;
        serde_json::from_str(
            &self
                .output
                .next_line()
                .await?
                .context("MCP reply, no legacy readiness lines")?,
        )
        .context("MCP JSON")
    }

    pub async fn environment(&mut self) -> Result<Value> {
        let reply = self.request(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"environment","arguments":{}}})).await?;
        serde_json::from_str(
            reply["result"]["content"][0]["text"]
                .as_str()
                .context("environment content")?,
        )
        .context("environment JSON")
    }

    pub async fn shutdown(mut self) -> Result<()> {
        drop(self.input);
        assert!(self.child.wait().await?.success());
        Ok(())
    }
}
