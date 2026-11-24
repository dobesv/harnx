use super::fixture::{bash, python, Fixture};
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::{JoinHandle, JoinSet},
};

struct Server {
    url: String,
    hits: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Server {
    fn endpoint(&self, path: &str) -> String {
        format!("{}/{path}", self.url)
    }

    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let count = hits.clone();
        let task = tokio::spawn(async move {
            let mut tasks = JoinSet::new();
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        let (stream, _) = result.unwrap();
                        count.fetch_add(1, Ordering::SeqCst);
                        tasks.spawn(serve(stream));
                    }
                    Some(_) = tasks.join_next() => {}
                }
            }
        });
        Self { url, hits, task }
    }
}

async fn serve(mut stream: TcpStream) {
    let mut request = vec![0; 8192];
    let count = stream.read(&mut request).await.unwrap();
    let request = String::from_utf8_lossy(&request[..count]);
    let path = request.split_whitespace().nth(1).unwrap();
    if path == "/slow-headers" {
        std::future::pending::<()>().await;
    }
    let response = match path {
        "/redirect" => "HTTP/1.1 302 Found\r\nLocation: http://localhost:1/forbidden\r\nContent-Length: 0\r\n\r\n".into(),
        "/size" => "HTTP/1.1 200 OK\r\nContent-Length: 262145\r\n\r\n".into(),
        "/chunked" => format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n40000\r\n{}\r\n1\r\nx\r\n0\r\n\r\n", "x".repeat(262144)),
        "/limit" => format!("HTTP/1.1 200 OK\r\nContent-Length: 262144\r\n\r\n{}", "x".repeat(262144)),
        "/slow-body" => "HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n".into(),
        "/invalid" => "HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\nÿ".into(),
        "/status" => "HTTP/1.1 403 Forbidden\r\nContent-Length: 2\r\n\r\nno".into(),
        _ => "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".into(),
    };
    let _ = stream.write_all(response.as_bytes()).await;
    if path == "/slow-body" {
        std::future::pending::<()>().await;
    }
}

fn entries(url: &str) -> [Value; 2] {
    [
        bash(&format!(
            "fetch '{url}' | jq '{{output: ., is_error: false}}' | respond"
        )),
        python(&format!(
            "respond({{'output': fetch(url='{url}'), 'is_error': False}})"
        )),
    ]
}

async fn evaluate(mut entry: Value, hosts: Value) -> (Value, usize) {
    entry["fetch"] = hosts;
    let fixture = Fixture::new(vec![entry]).await;
    let result = fixture.eval(json!({})).await.unwrap();
    (result.output, fixture.calls().len())
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_fetch_allowed_host_bash_and_python() {
    let server = Server::start().await;
    for entry in entries(&server.url) {
        let (output, calls) = evaluate(entry, json!(["127.0.0.1"])).await;
        assert_eq!(output, json!({"status":200,"body":"ok"}));
        assert_eq!(calls, 0);
    }
    assert_eq!(server.hits.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_fetch_denies_unlisted_or_missing_allowlist_without_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    for entry in entries(&url) {
        for hosts in [json!([]), json!(["localhost"]), json!(["127.0.0.10"])] {
            let (output, calls) = evaluate(entry.clone(), hosts).await;
            assert_eq!(output["is_error"], true, "{output}");
            assert!(output.to_string().contains("fetch denied"), "{output}");
            assert_eq!(calls, 0);
        }
        let fixture = Fixture::new(vec![entry]).await;
        let result = fixture.eval(json!({})).await.unwrap();
        assert!(result
            .output
            .to_string()
            .contains("no fetch host allowlist"));
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_fetch_size_limits_content_length_and_streaming() {
    let server = Server::start().await;
    for path in ["size", "chunked"] {
        for entry in entries(&server.endpoint(path)) {
            let (output, calls) = evaluate(entry, json!(["127.0.0.1"])).await;
            assert!(output.to_string().contains("262144-byte limit"), "{output}");
            assert_eq!(output["is_error"], true);
            assert_eq!(calls, 0);
        }
    }
    for entry in entries(&format!("{}/limit", server.url)) {
        let (output, _) = evaluate(entry, json!(["127.0.0.1"])).await;
        assert_eq!(output["status"], 200, "{output}");
        assert_eq!(output["body"].as_str().unwrap().len(), 262144);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_fetch_timeout_covers_headers_and_body() {
    let server = Server::start().await;
    // Concurrent independent frames keep this test near 10s while covering both languages.
    let mut tasks = JoinSet::new();
    for path in ["slow-headers", "slow-body"] {
        for entry in entries(&server.endpoint(path)) {
            tasks.spawn(evaluate(entry, json!(["127.0.0.1"])));
        }
    }
    while let Some(result) = tasks.join_next().await {
        let (output, calls) = result.unwrap();
        assert!(
            output.to_string().contains("fetch timed out after 10s"),
            "{output}"
        );
        assert_eq!(calls, 0);
    }
    assert_eq!(server.hits.load(Ordering::SeqCst), 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_fetch_does_not_follow_redirects_and_returns_http_errors() {
    let server = Server::start().await;
    for (path, expected) in [
        ("redirect", json!({"status":302,"body":""})),
        ("status", json!({"status":403,"body":"no"})),
    ] {
        for entry in entries(&server.endpoint(path)) {
            let (output, _) = evaluate(entry, json!(["127.0.0.1"])).await;
            assert_eq!(output, expected);
        }
    }
    assert_eq!(server.hits.load(Ordering::SeqCst), 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_fetch_rejects_unsafe_urls_and_bad_arguments() {
    for url in [
        "file:///etc/passwd",
        "ftp://127.0.0.1/a",
        "http://user:secret@127.0.0.1/",
        "relative/path",
    ] {
        for entry in entries(url) {
            let (output, calls) = evaluate(entry, json!(["127.0.0.1"])).await;
            assert_eq!(output["is_error"], true, "{output}");
            assert_eq!(calls, 0);
        }
    }
    for entry in [
        bash("fetch"),
        bash("fetch x y"),
        python("fetch()"),
        python("fetch(url=1)"),
        python("fetch('http://127.0.0.1', method='POST')"),
    ] {
        let (output, _) = evaluate(entry, json!(["127.0.0.1"])).await;
        assert_eq!(output["is_error"], true, "{output}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_fetch_keeps_alternative_network_commands_unavailable() {
    for name in ["curl", "wget", "http"] {
        let (output, calls) = evaluate(
            bash(&format!("{name} http://127.0.0.1:1/")),
            json!(["127.0.0.1"]),
        )
        .await;
        assert_eq!(output["is_error"], true, "{output}");
        assert!(output.to_string().contains("127"), "{output}");
        assert_eq!(calls, 0);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_fetch_allow_policy_on_error_and_entry_isolation() {
    for mut entry in entries("http://127.0.0.1:1/") {
        entry["on_error"] = json!("allow");
        let (output, calls) = evaluate(entry, json!([])).await;
        assert_eq!(output, json!({}));
        assert_eq!(calls, 1);
    }
    let mut outer = bash("next");
    outer["name"] = json!("outer");
    outer["fetch"] = json!(["127.0.0.1"]);
    let fixture = Fixture::new(vec![outer, python("fetch('http://127.0.0.1:1/')")]).await;
    let output = fixture.eval(json!({})).await.unwrap().output;
    assert!(
        output.to_string().contains("no fetch host allowlist"),
        "{output}"
    );
    assert!(fixture.calls().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_fetch_documentation_examples_execute_verbatim() {
    let server = Server::start().await;
    let examples = super::documentation::blocks();
    // The guide's fetch yaml examples follow the interact section's two
    // bash/python examples.
    for (_, source) in examples.iter().skip(12) {
        for path in ["allowed", "status"] {
            let yaml = source
                .replace("https://policy.example.com/allowed", &server.endpoint(path))
                .replace("fetch: [policy.example.com]", "fetch: [127.0.0.1]");
            let config: harnx_core::config_data::ConfigData = serde_yaml::from_str(&yaml).unwrap();
            let values = serde_json::to_value(config.middleware).unwrap();
            let fixture = Fixture::new(values.as_array().unwrap().clone()).await;
            let output = fixture.eval(json!({"value":"kept"})).await.unwrap().output;
            if path == "allowed" {
                assert_eq!(output, json!({"value":"kept"}));
                assert_eq!(fixture.calls().len(), 1);
            } else {
                assert_eq!(output["is_error"], true, "{output}");
                assert!(fixture.calls().is_empty());
            }
        }
    }
    assert_eq!(server.hits.load(Ordering::SeqCst), 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_fetch_dns_grants_and_utf8_errors() {
    let server = Server::start().await;
    let url = server.url.replace("127.0.0.1", "LOCALHOST");
    for entry in entries(&url) {
        let (output, _) = evaluate(entry, json!(["LOCALHOST"])).await;
        assert_eq!(output["status"], 200, "{output}");
    }
    for entry in entries(&server.endpoint("invalid")) {
        let (output, calls) = evaluate(entry, json!(["127.0.0.1"])).await;
        assert!(output.to_string().contains("not UTF-8"), "{output}");
        assert_eq!(calls, 0);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_fetch_metadata_literal_urls_are_denied() {
    for host in [
        "169.254.169.254",
        "100.100.100.200",
        "[::ffff:169.254.169.254]",
        "[fd00:ec2::254]",
        "[fe80::1]",
    ] {
        for entry in entries(&format!("http://{host}/")) {
            let (output, calls) = evaluate(entry, json!([host])).await;
            assert!(
                output.to_string().contains("fetch address blocked"),
                "{output}"
            );
            assert_eq!(calls, 0);
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_fetch_host_failure_cannot_be_caught() {
    for entry in [
        bash("set +e; fetch http://127.0.0.1/ || true; next"),
        python("try:\n    fetch('http://127.0.0.1/')\nexcept Exception:\n    pass\ncall_next()"),
    ] {
        let (output, calls) = evaluate(entry, json!([])).await;
        assert_eq!(output["is_error"], true, "{output}");
        assert_eq!(calls, 0);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_fetch_shorter_middleware_timeout_wins() {
    let server = Server::start().await;
    for mut entry in entries(&server.endpoint("slow-body")) {
        entry["timeout"] = json!("100ms");
        let started = std::time::Instant::now();
        let (output, calls) = evaluate(entry, json!(["127.0.0.1"])).await;
        // The deadline cancels the host wait; either diagnostic can win that race.
        let error = output["error"].as_str().unwrap();
        assert!(
            error.contains("middleware timed out") || error.contains("fetch cancelled"),
            "{output}"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(output["is_error"], true);
        assert_eq!(calls, 0);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_fetch_cancellation_stops_request_even_with_allow_policy() {
    let server = Server::start().await;
    for mut entry in entries(&server.endpoint("slow-body")) {
        entry["fetch"] = json!(["127.0.0.1"]);
        entry["on_error"] = json!("allow");
        let fixture = Fixture::new(vec![entry]).await;
        let abort = harnx_core::abort::create_abort_signal();
        let before = server.hits.load(Ordering::SeqCst);
        let run = harnx_engine::tool::eval_tool_calls(
            &fixture.ctx,
            vec![super::fixture::call(json!({}))],
            &abort,
        );
        tokio::pin!(run);
        tokio::select! {
            result = &mut run => panic!("fetch finished before abort: {result:?}"),
            () = async { while server.hits.load(Ordering::SeqCst) == before { tokio::time::sleep(Duration::from_millis(10)).await; } } => {},
            () = tokio::time::sleep(Duration::from_secs(5)) => panic!("fetch never connected"),
        }
        abort.set_ctrlc();
        assert!(tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .unwrap()
            .is_err());
        assert!(fixture.calls().is_empty());
    }
}
