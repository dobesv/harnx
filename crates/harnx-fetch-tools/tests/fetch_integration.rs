use std::collections::HashMap;

use harnx_fetch_tools::server::{FetchParams, FetchServer, YoutubeTranscriptParams};
use harnx_fetch_tools::MAX_RESPONSE_BYTES;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn params(url: String) -> FetchParams {
    FetchParams {
        url,
        max_length: Some(0),
        max_output_bytes: Some(100_000),
        ..FetchParams::default()
    }
}

fn text(result: &rmcp::model::CallToolResult) -> &str {
    result
        .content
        .iter()
        .find_map(|content| content.as_text().map(|text| text.text.as_str()))
        .expect("tool result contains text")
}

async fn server_with(path_name: &str, body: &str, content_type: &str) -> (MockServer, String) {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(path_name))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, content_type))
        .expect(1)
        .mount(&mock)
        .await;
    let url = format!("{}{path_name}", mock.uri());
    (mock, url)
}

#[tokio::test]
async fn html_markdown_text_and_json_happy_paths() {
    let html = "<html><body><h1>Hello</h1><script>hidden()</script><p>Rust world</p></body></html>";
    for tool in ["html", "markdown", "txt"] {
        let (_mock, url) = server_with("/page", html, "text/html").await;
        let server = FetchServer::new(true);
        let result = match tool {
            "html" => server.fetch_html_impl(params(url)).await,
            "markdown" => server.fetch_markdown_impl(params(url)).await,
            "txt" => server.fetch_txt_impl(params(url)).await,
            _ => unreachable!(),
        }
        .unwrap();
        match tool {
            "html" => assert!(text(&result).contains("<h1>Hello</h1>")),
            "markdown" => assert!(text(&result).contains("# Hello")),
            "txt" => {
                assert!(text(&result).contains("Hello Rust world"));
                assert!(!text(&result).contains("hidden"));
            }
            _ => unreachable!(),
        }
    }

    let (_mock, url) =
        server_with("/data", r#"{"ok":true,"items":[1,2]}"#, "application/json").await;
    let result = FetchServer::new(true)
        .fetch_json_impl(params(url))
        .await
        .unwrap();
    assert_eq!(
        text(&result),
        "{\n  \"ok\": true,\n  \"items\": [\n    1,\n    2\n  ]\n}"
    );
}

#[tokio::test]
async fn sends_custom_headers() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/headers"))
        .and(header("x-fetch-test", "present"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .expect(1)
        .mount(&mock)
        .await;
    let mut request = params(format!("{}/headers", mock.uri()));
    request
        .headers
        .insert("x-fetch-test".to_owned(), "present".to_owned());
    let result = FetchServer::new(true)
        .fetch_html_impl(request)
        .await
        .unwrap();
    assert_eq!(text(&result), "ok");
}

#[tokio::test]
async fn readable_extracts_article_fixture() {
    let (_mock, url) = server_with(
        "/article",
        include_str!("fixtures/article.html"),
        "text/html",
    )
    .await;
    let result = FetchServer::new(true)
        .fetch_readable_impl(params(url))
        .await
        .unwrap();
    assert!(text(&result).contains("Native fetch tools keep HTTP policy"));
    assert!(!text(&result).contains("window.secret"));
}

#[tokio::test]
async fn youtube_direct_path_fetches_selected_caption_track() {
    let mock = MockServer::start().await;
    let caption_url = format!("{}/captions?lang=en", mock.uri());
    let player = serde_json::json!({
        "captions": {
            "playerCaptionsTracklistRenderer": {
                "captionTracks": [
                    {"baseUrl": caption_url, "languageCode": "en"},
                    {"baseUrl": "https://example.com/fr", "languageCode": "fr"}
                ]
            }
        }
    });
    let watch = format!("<script>ytInitialPlayerResponse = {player};</script>");
    Mock::given(method("GET"))
        .and(path("/watch"))
        .respond_with(ResponseTemplate::new(200).set_body_string(watch))
        .expect(1)
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/captions"))
        .and(query_param("lang", "en"))
        .and(query_param("fmt", "srv1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(include_str!("fixtures/transcript.xml"), "text/xml"),
        )
        .expect(1)
        .mount(&mock)
        .await;

    let result = FetchServer::new(true)
        .fetch_youtube_transcript_impl(YoutubeTranscriptParams {
            fetch: params(format!("{}/watch?v=test", mock.uri())),
            lang: "en".to_owned(),
        })
        .await
        .unwrap();
    assert_eq!(
        text(&result),
        "[00:01] First & important line\n[01:05] Second line"
    );
}

#[tokio::test]
async fn rejects_invalid_json_http_errors_and_oversized_bodies() {
    let (_mock, url) = server_with("/bad", "not json", "text/plain").await;
    assert!(FetchServer::new(true)
        .fetch_json_impl(params(url))
        .await
        .unwrap_err()
        .message
        .contains("not valid JSON"));

    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/error"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&mock)
        .await;
    assert!(FetchServer::new(true)
        .fetch_html_impl(params(format!("{}/error", mock.uri())))
        .await
        .unwrap_err()
        .message
        .contains("503"));

    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/large"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'x'; MAX_RESPONSE_BYTES + 1]))
        .mount(&mock)
        .await;
    assert!(FetchServer::new(true)
        .fetch_html_impl(params(format!("{}/large", mock.uri())))
        .await
        .unwrap_err()
        .message
        .contains("exceeds"));
}

#[tokio::test]
async fn enforces_redirect_limit() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/loop"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/loop"))
        .mount(&mock)
        .await;
    let error = FetchServer::new(true)
        .fetch_html_impl(params(format!("{}/loop", mock.uri())))
        .await
        .unwrap_err();
    assert!(error.message.contains("redirect"), "{}", error.message);
}

#[tokio::test]
async fn compatibility_and_smart_truncation_run_after_transform() {
    let (_mock, url) = server_with("/text", "abcdefghij", "text/plain").await;
    let mut request = params(url);
    request.start_index = Some(2);
    request.max_length = Some(5);
    request.max_output_bytes = Some(4);
    request.headers = HashMap::new();
    let output = FetchServer::new(true)
        .fetch_html_impl(request)
        .await
        .unwrap();
    assert!(text(&output).len() <= 4 || text(&output).contains("truncated"));
}

#[tokio::test]
async fn minified_json_uses_byte_truncation_without_per_line_clipping() {
    let payload = "a".repeat(8_000);
    let body = serde_json::json!({"payload": payload}).to_string();
    let (_mock, url) = server_with("/minified.json", &body, "application/json").await;
    let mut request = params(url);
    request.max_output_bytes = Some(3_000);

    let result = FetchServer::new(true)
        .fetch_json_impl(request)
        .await
        .unwrap();
    let output = text(&result);
    assert!(output.contains("Use max_output_bytes to increase limit"));
    assert!(output.contains("\"payload\": \"aaaa"));
    assert!(output.ends_with("aaaa\"\n}"));
    assert!(
        !output.contains("bytes removed from line"),
        "per-line clipping mangled minified JSON: {output}"
    );
}

#[tokio::test]
async fn readable_without_article_returns_explicit_extraction_error() {
    let (_mock, url) = server_with(
        "/unreadable",
        include_str!("fixtures/unreadable.html"),
        "text/html",
    )
    .await;
    let error = FetchServer::new(true)
        .fetch_readable_impl(params(url))
        .await
        .expect_err("navigation-only page must not become readable output");
    assert!(
        error.message.contains("readability extraction"),
        "{}",
        error.message
    );
    assert!(!error.message.contains("window.noArticle"));
}

#[tokio::test]
async fn youtube_unavailable_page_returns_clear_player_response_error() {
    let (_mock, url) = server_with(
        "/watch",
        include_str!("fixtures/youtube_unavailable.html"),
        "text/html",
    )
    .await;
    let error = FetchServer::new(true)
        .fetch_youtube_transcript_impl(YoutubeTranscriptParams {
            fetch: params(url),
            lang: "en".to_owned(),
        })
        .await
        .expect_err("consent page must not become a transcript");
    assert!(
        error
            .message
            .contains("YouTube player response was not found"),
        "{}",
        error.message
    );
}
