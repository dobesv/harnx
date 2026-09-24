use harnx_exa_tools::server::{ExaServer, WebFetchParams, WebSearchParams};
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SEARCH_RESPONSE: &str = include_str!("fixtures/search_response.json");
const SEARCH_OUTPUT: &str = include_str!("fixtures/search_output.txt");
const CONTENTS_RESPONSE: &str = include_str!("fixtures/contents_response.json");
const CONTENTS_OUTPUT: &str = include_str!("fixtures/contents_output.txt");

fn text_content(result: &rmcp::model::CallToolResult) -> &str {
    result
        .content
        .iter()
        .find_map(|content| content.as_text().map(|text| text.text.as_str()))
        .expect("tool result must contain text")
}

fn set_api_key() {
    // SAFETY: nextest runs each test in a separate process.
    unsafe { std::env::set_var("EXA_API_KEY", "test-exa-key") };
}

#[tokio::test]
async fn search_posts_expected_body_headers_and_formats_response() {
    set_api_key();
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/search"))
        .and(header("x-api-key", "test-exa-key"))
        .and(header("content-type", "application/json"))
        .and(header("x-exa-integration", "web-search-mcp"))
        .and(body_json(serde_json::json!({
            "query": "rust async",
            "type": "auto",
            "numResults": 3,
            "contents": {"highlights": true},
            "category": "publication"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_raw(SEARCH_RESPONSE, "application/json"))
        .expect(1)
        .mount(&mock_server)
        .await;

    let result = ExaServer::with_base_url(mock_server.uri())
        .web_search_impl(WebSearchParams {
            query: "rust category:Publication async".into(),
            num_results: Some(3),
        })
        .await
        .expect("search must succeed");

    assert_eq!(result.is_error, Some(false));
    assert_eq!(text_content(&result), SEARCH_OUTPUT.trim_end());
}

#[tokio::test]
async fn contents_posts_top_level_options_headers_and_formats_response() {
    set_api_key();
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/contents"))
        .and(header("x-api-key", "test-exa-key"))
        .and(header("content-type", "application/json"))
        .and(header("x-exa-integration", "crawling-mcp"))
        .and(body_json(serde_json::json!({
            "urls": ["https://example.com/async-rust", "https://example.com/untitled"],
            "text": {"maxCharacters": 5000}
        })))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(CONTENTS_RESPONSE, "application/json"),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    let result = ExaServer::with_base_url(mock_server.uri())
        .web_fetch_impl(WebFetchParams {
            urls: vec![
                "https://example.com/async-rust".into(),
                "https://example.com/untitled".into(),
            ],
            max_characters: Some(5000),
        })
        .await
        .expect("fetch must succeed");

    assert_eq!(result.is_error, Some(false));
    assert_eq!(text_content(&result), CONTENTS_OUTPUT.trim_end());
}

#[tokio::test]
async fn empty_search_and_contents_are_success_results() {
    set_api_key();
    for (endpoint, integration) in [("/search", "web-search-mcp"), ("/contents", "crawling-mcp")] {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(endpoint))
            .and(header("x-exa-integration", integration))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                include_str!("fixtures/empty_results.json"),
                "application/json",
            ))
            .expect(1)
            .mount(&mock_server)
            .await;
        let server = ExaServer::with_base_url(mock_server.uri());
        let result = if endpoint == "/search" {
            server
                .web_search_impl(WebSearchParams {
                    query: "nothing".into(),
                    num_results: None,
                })
                .await
        } else {
            server
                .web_fetch_impl(WebFetchParams {
                    urls: vec!["https://empty.test".into()],
                    max_characters: None,
                })
                .await
        }
        .expect("empty response is successful");
        let expected = if endpoint == "/search" {
            "No search results found. Please try a different query."
        } else {
            "No content found for the provided URL(s)."
        };
        assert_eq!(text_content(&result), expected);
    }
}

#[tokio::test]
async fn empty_contents_with_url_errors_is_recoverable() {
    set_api_key();
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/contents"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            include_str!("fixtures/contents_empty_error.json"),
            "application/json",
        ))
        .expect(1)
        .mount(&mock_server)
        .await;

    let error = ExaServer::with_base_url(mock_server.uri())
        .web_fetch_impl(WebFetchParams {
            urls: vec!["https://missing.test".into()],
            max_characters: None,
        })
        .await
        .expect_err("URL errors without results must fail");
    assert_eq!(
        error.message,
        "Error fetching URL(s): https://missing.test: CRUX_ERROR; https://unknown.test: unknown error"
    );
}

#[tokio::test]
async fn maps_auth_rate_limit_and_other_http_statuses() {
    set_api_key();
    for (status, expected) in [
        (401, "Invalid or unauthorized API key"),
        (403, "Invalid or unauthorized API key"),
        (429, "rate limit exceeded"),
        (500, "status 500"),
    ] {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .respond_with(ResponseTemplate::new(status))
            .expect(1)
            .mount(&mock_server)
            .await;
        let error = ExaServer::with_base_url(mock_server.uri())
            .web_search_impl(WebSearchParams {
                query: "rust".into(),
                num_results: None,
            })
            .await
            .expect_err("HTTP status must be recoverable");
        assert!(
            error.message.contains(expected),
            "expected {:?} in {:?}",
            expected,
            error.message
        );
    }
}
