use harnx_fetch_tools::FetchToolset;
use harnx_toolset::{
    ToolInvocation, ToolInvocationContext, ToolInvokeError, Toolset, CAPABILITY_STRUCTURED_RESULT,
};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn serve(path_name: &str, response: ResponseTemplate) -> (MockServer, String) {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(path_name))
        .respond_with(response)
        .mount(&mock)
        .await;
    let url = format!("{}{path_name}", mock.uri());
    (mock, url)
}

async fn page(path_name: &str, content: &str, content_type: &str) -> (MockServer, String) {
    serve(
        path_name,
        ResponseTemplate::new(200).set_body_raw(content, content_type),
    )
    .await
}

async fn call(tool: &str, args: Value, script: bool) -> Result<Value, ToolInvokeError> {
    let mut context = ToolInvocationContext::default();
    if script {
        context
            .capabilities
            .insert(CAPABILITY_STRUCTURED_RESULT.to_string());
    }
    FetchToolset::with_allow_private_ip(true)
        .invoke_with_context(ToolInvocation {
            tool: tool.to_owned(),
            args,
            context,
            cancel: CancellationToken::new(),
        })
        .await
}

async fn script(tool: &str, args: Value) -> Value {
    call(tool, args, true).await.unwrap()["structuredContent"].clone()
}

#[tokio::test]
async fn script_fetch_returns_the_body_as_text() {
    let (_mock, url) = page("/page", "hello world", "text/plain").await;
    let fields = script("fetch_txt", json!({"url": url})).await;
    assert_eq!(fields["text"], "hello world");
    assert_eq!(fields["notes"], json!([]));
    assert_eq!(fields["url"], url);
    assert_eq!(fields["final_url"], url);
    assert_eq!(fields["content_type"], "text/plain");
}

#[tokio::test]
async fn script_fetch_reports_a_max_length_cut() {
    let (_mock, url) = page("/page", "0123456789abcdefghij", "text/plain").await;
    let fields = script("fetch_txt", json!({"url": url, "max_length": 10})).await;
    assert_eq!(fields["text"], "0123456789");
    assert_eq!(
        fields["notes"],
        json!(["10 more characters. Use start_index=10 to continue"])
    );
}

#[tokio::test]
async fn script_fetch_json_cut_short_says_it_is_not_json() {
    let (_mock, url) = page(
        "/data",
        r#"{"alpha": "0123456789", "beta": 2}"#,
        "application/json",
    )
    .await;
    let fields = script("fetch_json", json!({"url": url, "max_length": 12})).await;
    let text = fields["text"].as_str().unwrap();
    assert!(!text.contains("[JSON preview"), "{text}");
    let notes = fields["notes"].as_array().unwrap();
    assert!(notes
        .iter()
        .any(|note| note == "output is a cut JSON preview, not valid JSON"));
    assert!(notes[0].as_str().unwrap().contains("start_index="));
}

#[tokio::test]
async fn script_fetch_over_the_byte_cap_keeps_whole_lines() {
    let (_mock, url) = page("/page", "aaaa\nbbbb\ncccc\n", "text/plain").await;
    let fields = script(
        "fetch_html",
        json!({"url": url, "max_output_bytes": 12, "head_lines": 1}),
    )
    .await;
    assert_eq!(fields["text"], "aaaa\nbbbb\n");
    assert_eq!(
        fields["notes"],
        json!([
            "5 more characters. Use start_index=10 to continue",
            "cut at 10 of 15 bytes. Use max_output_bytes to see more",
            "head_lines is ignored in a script; pipe to head"
        ])
    );
}

#[tokio::test]
async fn script_fetch_non_2xx_is_still_an_error() {
    let (_mock, url) = serve("/error", ResponseTemplate::new(503)).await;
    match call("fetch_html", json!({"url": url}), true).await {
        Err(ToolInvokeError::Recoverable(message)) => assert!(message.contains("503")),
        other => panic!("expected a recoverable error, got {other:?}"),
    }
}

#[tokio::test]
async fn direct_fetch_results_are_unchanged() {
    let (_mock, url) = page("/page", "0123456789abcdefghij", "text/plain").await;
    let reply = call("fetch_txt", json!({"url": url}), false).await.unwrap();
    assert!(reply.get("structuredContent").is_none());
    let reply = call("fetch_txt", json!({"url": url, "max_length": 10}), false)
        .await
        .unwrap();
    assert!(reply.get("structuredContent").is_none());
    assert_eq!(reply["content"][0]["text"], "0123456789");

    let (_mock, url) = page("/data", r#"{"alpha": "0123456789"}"#, "application/json").await;
    let reply = call("fetch_json", json!({"url": url, "max_length": 12}), false)
        .await
        .unwrap();
    assert!(reply.get("structuredContent").is_none());
    assert!(reply["content"][0]["text"]
        .as_str()
        .unwrap()
        .starts_with("[JSON preview: output truncated by max_length/start_index]\n"));

    let (_mock, url) = page("/page", "aaaa\nbbbb\ncccc\n", "text/plain").await;
    let reply = call(
        "fetch_txt",
        json!({"url": url, "max_output_bytes": 12}),
        false,
    )
    .await
    .unwrap();
    assert!(reply.get("structuredContent").is_none());

    let (_mock, url) = serve("/error", ResponseTemplate::new(503)).await;
    assert!(matches!(
        call("fetch_html", json!({"url": url}), false).await,
        Err(ToolInvokeError::Recoverable(message)) if message.contains("503")
    ));
}

#[tokio::test]
async fn all_fetch_tools_declare_the_page_mapping() {
    let tools = FetchToolset::with_allow_private_ip(false).tools();
    assert_eq!(tools.len(), 6);
    for tool in tools {
        let output = tool
            .script_output()
            .unwrap_or_else(|| panic!("{}", tool.name));
        assert_eq!(output.stdout.as_deref(), Some("text"), "{}", tool.name);
        assert_eq!(output.stderr.as_deref(), Some("notes"), "{}", tool.name);
        assert!(output.summary.is_some(), "{}", tool.name);
    }
}

#[tokio::test]
async fn script_fetch_continues_from_a_non_zero_start_index() {
    let (_mock, url) = page("/page", "0123456789abcdefghij", "text/plain").await;
    let args = json!({"url": url, "start_index": 4, "max_length": 6});
    let fields = script("fetch_txt", args).await;
    assert_eq!(fields["text"], "456789");
    assert_eq!(
        fields["notes"],
        json!(["10 more characters. Use start_index=10 to continue"])
    );
}

#[tokio::test]
async fn script_fetch_start_index_follows_what_the_byte_cap_printed() {
    let body = "ééé\n".repeat(5);
    let (_mock, url) = page("/page", &body, "text/plain").await;
    let first = script("fetch_html", json!({"url": url, "max_output_bytes": 20})).await;
    // Two 7-byte lines fit; the window itself holds all 20 characters.
    assert_eq!(first["text"], "ééé\nééé\n");
    let notes = first["notes"].as_array().unwrap();
    assert!(
        notes
            .iter()
            .any(|note| note == "12 more characters. Use start_index=8 to continue"),
        "{notes:?}"
    );
    assert!(notes
        .iter()
        .any(|note| note.as_str().unwrap().starts_with("cut at 14 of 35 bytes")));
    let second = script(
        "fetch_html",
        json!({"url": url, "start_index": 8, "max_output_bytes": 20}),
    )
    .await;
    assert_eq!(second["text"], "ééé\nééé\n");
    let rest = script("fetch_html", json!({"url": url, "start_index": 16})).await;
    let joined = format!(
        "{}{}{}",
        first["text"].as_str().unwrap(),
        second["text"].as_str().unwrap(),
        rest["text"].as_str().unwrap()
    );
    assert_eq!(joined, body);
}

#[tokio::test]
async fn script_fetch_notes_that_line_caps_are_ignored() {
    let (_mock, url) = page("/page", "one\ntwo\n", "text/plain").await;
    let fields = script("fetch_html", json!({"url": url, "tail_lines": 1})).await;
    assert_eq!(
        fields["notes"],
        json!(["tail_lines is ignored in a script; pipe to tail"])
    );
}
