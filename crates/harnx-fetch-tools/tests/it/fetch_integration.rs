use std::collections::HashMap;
use std::time::Duration;

use harnx_core::crypto::base64_decode;
use harnx_fetch_tools::server::{FetchParams, FetchServer, YoutubeTranscriptParams};
use harnx_fetch_tools::{FetchToolset, MAX_RESPONSE_BYTES};
use harnx_toolset::Toolset;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
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
    assert_eq!(result.content.len(), 1);
    &result.content[0].as_text().expect("text result").text
}

async fn server_with(path_name: &str, body: &str, content_type: &str) -> (MockServer, String) {
    server_with_response(
        path_name,
        ResponseTemplate::new(200).set_body_raw(body, content_type),
        1,
    )
    .await
}

async fn server_with_response(
    path_name: &str,
    response: ResponseTemplate,
    expected_requests: u64,
) -> (MockServer, String) {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(path_name))
        .respond_with(response)
        .expect(expected_requests)
        .mount(&mock)
        .await;
    let url = format!("{}{path_name}", mock.uri());
    (mock, url)
}

// Signature fixtures: classification doesn't parse or decode the rest of the file.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\xFF";
const IMAGE_FIXTURES: [(&str, &[u8]); 5] = [
    ("image/png", PNG),
    ("image/jpeg", b"\xFF\xD8\xFF\xE0\0\x10JFIF\0"),
    ("image/gif", b"GIF87a\x01\0\x01\0"),
    ("image/gif", b"GIF89a\x01\0\x01\0"),
    ("image/webp", b"RIFF\x04\0\0\0WEBP"),
];

async fn fetch_image_reply(toolset: &FetchToolset, name: &str, url: &str) -> Value {
    toolset
        .invoke(
            name,
            json!({"url": url, "start_index": 1000, "max_length": 1, "max_output_bytes": 1}),
            CancellationToken::new(),
        )
        .await
        .unwrap_or_else(|error| panic!("{name} failed for {url}: {error}"))
}

fn assert_image_reply(reply: Value, final_url: &str, mime: &str, bytes: &[u8]) {
    assert_ne!(reply["isError"], true);
    let content = reply["content"]
        .as_array()
        .expect("top-level content array");
    assert_eq!(content.len(), 2);
    let data = content[0]["data"].as_str().expect("base64 image data");
    assert_eq!(base64_decode(data).unwrap(), bytes);
    assert_eq!(
        content[0],
        json!({"type": "image", "data": data, "mimeType": mime, "annotations": {"audience": ["assistant"]}})
    );
    let summary = format!(
        "Fetched image '{final_url}' ({mime}, {} bytes)",
        bytes.len()
    );
    assert_eq!(
        content[1],
        json!({"type": "text", "text": summary, "annotations": {"audience": ["user"]}})
    );
}

#[tokio::test]
async fn all_page_fetchers_return_supported_images_with_audience_summary() {
    let tools = [
        "fetch_html",
        "fetch_markdown",
        "fetch_txt",
        "fetch_json",
        "fetch_readable",
    ];
    let toolset = FetchToolset::with_allow_private_ip(true);
    for (mime, body) in IMAGE_FIXTURES {
        let (_mock, url) = server_with_response(
            "/image",
            ResponseTemplate::new(200).set_body_raw(body, mime),
            tools.len() as u64,
        )
        .await;
        for tool in tools {
            let reply = fetch_image_reply(&toolset, tool, &url).await;
            assert_image_reply(reply, &url, mime, body);
        }
    }
}

#[tokio::test]
async fn detects_png_with_octet_stream_or_absent_content_type() {
    let toolset = FetchToolset::with_allow_private_ip(true);
    for response in [
        ResponseTemplate::new(200).set_body_raw(PNG, "Application/Octet-Stream; ignored=1"),
        ResponseTemplate::new(200).set_body_bytes(PNG),
    ] {
        let (_mock, url) = server_with_response("/blob", response, 1).await;
        let reply = fetch_image_reply(&toolset, "fetch_markdown", &url).await;
        assert_image_reply(reply, &url, "image/png", PNG);
    }
}

#[tokio::test]
async fn image_result_uses_sniffed_mime_and_final_url() {
    let (mock, final_url) = server_with_response(
        "/asset",
        ResponseTemplate::new(200).set_body_raw(PNG, "IMAGE/JPEG; ignored=1"),
        1,
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/download"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/asset"))
        .expect(1)
        .mount(&mock)
        .await;
    let reply = fetch_image_reply(
        &FetchToolset::with_allow_private_ip(true),
        "fetch_html",
        &format!("{}/download", mock.uri()),
    )
    .await;
    assert_image_reply(reply, &final_url, "image/png", PNG);
}

#[tokio::test]
async fn explicit_text_type_keeps_lossy_text_even_with_png_magic() {
    let (_mock, url) = server_with_response(
        "/text.png",
        ResponseTemplate::new(200).set_body_raw(PNG, "text/html"),
        1,
    )
    .await;
    let result = FetchServer::new(true)
        .fetch_html_impl(params(url))
        .await
        .unwrap();
    // The existing text path sanitizes replacement characters and most controls.
    assert_eq!(text(&result), "PNG\r\n\n");
}

#[tokio::test]
async fn rejects_unsupported_image_signatures_and_html_labelled_as_png() {
    let server = FetchServer::new(true);
    let fixtures: [(&str, &[u8]); 3] = [
        (
            "image/svg+xml",
            b"<svg xmlns='http://www.w3.org/2000/svg'/>",
        ),
        ("image/avif", b"\0\0\0\x18ftypavif\0\0\0\0avifmif1"),
        ("image/png", b"<html>Not an image</html>"),
    ];
    for (mime, body) in fixtures {
        let (_mock, url) = server_with_response(
            "/image.png",
            ResponseTemplate::new(200).set_body_raw(body, mime),
            1,
        )
        .await;
        let error = server.fetch_html_impl(params(url)).await.unwrap_err();
        assert!(
            error.message.contains("unsupported image format"),
            "{error}"
        );
        assert!(error.message.contains(mime), "{error}");
        assert!(error.message.contains("PNG, JPEG, GIF, WebP"), "{error}");
    }
}

fn png_at_size(size: usize) -> Vec<u8> {
    let mut body = PNG.to_vec();
    body.resize(size, 0);
    body
}

#[tokio::test]
async fn image_size_limit_accepts_five_mib_and_rejects_six_mib() {
    let body = png_at_size(5 * 1024 * 1024);
    let (_mock, url) = server_with_response(
        "/limit",
        ResponseTemplate::new(200).set_body_raw(body.clone(), "image/png"),
        1,
    )
    .await;
    let toolset = FetchToolset::with_allow_private_ip(true);
    let reply = fetch_image_reply(&toolset, "fetch_txt", &url).await;
    assert_image_reply(reply, &url, "image/png", &body);

    let (_mock, url) = server_with_response(
        "/too-large",
        ResponseTemplate::new(200).set_body_raw(png_at_size(6 * 1024 * 1024), "image/png"),
        1,
    )
    .await;
    let error = FetchServer::new(true)
        .fetch_txt_impl(params(url))
        .await
        .unwrap_err();
    assert!(error.message.contains("image at"), "{error}");
    assert!(error.message.contains("too large"), "{error}");
    assert!(error.message.contains("6291456 bytes"), "{error}");
    assert!(error.message.contains("5 MiB"), "{error}");
}

#[tokio::test]
async fn image_over_stream_cap_is_rejected_before_end_of_body() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/big.png", listener.local_addr().unwrap());
    let sender = tokio::spawn(serve_unfinished_png(listener));
    let server = FetchServer::new(true);
    let result =
        tokio::time::timeout(Duration::from_secs(10), server.fetch_html_impl(params(url))).await;
    sender.abort();
    let error = result
        .expect("must reject before the server ends the body")
        .unwrap_err();
    assert!(
        error.message.contains(&format!(
            "response body exceeds {MAX_RESPONSE_BYTES} byte limit"
        )),
        "{error}"
    );
}

async fn serve_unfinished_png(listener: TcpListener) {
    let (stream, _) = listener.accept().await.unwrap();
    let mut stream = BufReader::new(stream);
    let mut line = String::new();
    while stream.read_line(&mut line).await.unwrap() != 0 && line != "\r\n" {
        line.clear();
    }
    let mut stream = stream.into_inner();
    let size = MAX_RESPONSE_BYTES + 1;
    let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nTransfer-Encoding: chunked\r\n\r\n{size:x}\r\n"
    );
    stream.write_all(headers.as_bytes()).await.unwrap();
    stream.write_all(&png_at_size(size)).await.unwrap();
    stream.write_all(b"\r\n").await.unwrap();
    // Keep the response open without the terminating chunk. Buffering the whole body would hang.
    std::future::pending::<()>().await;
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
        include_str!("../fixtures/article.html"),
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
    assert_youtube_transcript("text/html", "text/xml").await;
}

#[tokio::test]
async fn youtube_transcript_ignores_image_content_type_on_watch_and_captions() {
    assert_youtube_transcript("image/png", "image/jpeg").await;
}

async fn assert_youtube_transcript(watch_type: &str, caption_type: &str) {
    let mock = MockServer::start().await;
    let caption_url = format!("{}/captions?lang=en", mock.uri());
    let player = json!({
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
        .respond_with(ResponseTemplate::new(200).set_body_raw(watch, watch_type))
        .expect(1)
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/captions"))
        .and(query_param("lang", "en"))
        .and(query_param("fmt", "srv1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(include_str!("../fixtures/transcript.xml"), caption_type),
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
        include_str!("../fixtures/unreadable.html"),
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
        include_str!("../fixtures/youtube_unavailable.html"),
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
