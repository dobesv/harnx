use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};

pub(in super::super) struct CapturedProvider {
    pub base: String,
    shutdown: CancellationToken,
    task: AbortOnDropHandle<Result<Vec<Value>>>,
}

impl CapturedProvider {
    pub async fn start(
        calls: &'static [(&'static str, &'static str)],
        response: &'static str,
    ) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}/v1", listener.local_addr()?);
        let shutdown = CancellationToken::new();
        let stop = shutdown.clone();
        let task = AbortOnDropHandle::new(tokio::spawn(async move {
            let mut requests = Vec::new();
            loop {
                let (mut socket, _) = tokio::select! {
                    _ = stop.cancelled() => return Ok(requests),
                    accepted = listener.accept() => accepted?,
                };
                let request = read_http_json(&mut socket).await?;
                let first = requests.is_empty();
                let message = if first {
                    initial_calls(&request, calls)?
                } else {
                    json!({"role": "assistant", "content": response})
                };
                let body = json!({"id": "result-test", "object": "chat.completion", "created": 1,
                    "model": "test", "choices": [{"index": 0, "message": message,
                        "finish_reason": if first { "tool_calls" } else { "stop" }}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                })
                .to_string();
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await?;
                requests.push(request);
            }
        }));
        Ok(Self {
            base,
            shutdown,
            task,
        })
    }

    pub async fn finish(self) -> Result<Vec<Value>> {
        self.shutdown.cancel();
        tokio::time::timeout(CI_SAFE_TIMEOUT, self.task).await??
    }
}

fn initial_calls(request: &Value, specs: &[(&str, &str)]) -> Result<Value> {
    let mut calls = Vec::new();
    for &(suffix, id) in specs {
        let name = request["tools"]
            .as_array()
            .context("tool declarations")?
            .iter()
            .find_map(|tool| {
                tool["function"]["name"]
                    .as_str()
                    .filter(|name| name.ends_with(suffix))
            })
            .with_context(|| format!("missing {suffix}"))?;
        calls.push(
            json!({"id": id, "type": "function", "function": {"name": name, "arguments": "{}"}}),
        );
    }
    Ok(json!({"role": "assistant", "content": null, "tool_calls": calls}))
}

async fn read_http_json(socket: &mut tokio::net::TcpStream) -> Result<Value> {
    let mut request = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let count = socket.read(&mut buffer).await?;
        anyhow::ensure!(count != 0, "provider request closed before body");
        request.extend_from_slice(&buffer[..count]);
        if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
            let length: usize = String::from_utf8_lossy(&request[..end])
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse().unwrap())
                })
                .context("provider request content-length")?;
            if request.len() >= end + 4 + length {
                return Ok(serde_json::from_slice(&request[end + 4..end + 4 + length])?);
            }
        }
    }
}
