use crate::server::model::{ContentsResponse, SearchResponse, UrlStatus};

pub const NO_SEARCH_RESULTS: &str = "No search results found. Please try a different query.";
pub const NO_CONTENT: &str = "No content found for the provided URL(s).";

pub fn format_search(response: &SearchResponse) -> String {
    if response.results.is_empty() {
        return NO_SEARCH_RESULTS.to_owned();
    }

    response
        .results
        .iter()
        .map(|result| {
            let mut lines = vec![
                format!("Title: {}", result.title.as_deref().unwrap_or("N/A")),
                format!("URL: {}", result.url),
                format!(
                    "Published: {}",
                    result.published_date.as_deref().unwrap_or("N/A")
                ),
                format!("Author: {}", result.author.as_deref().unwrap_or("N/A")),
            ];
            if !result.highlights.is_empty() {
                lines.push(format!("Highlights:\n{}", result.highlights.join("\n")));
            } else if let Some(text) = &result.text {
                lines.push(format!("Text: {text}"));
            }
            lines.join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n\n---\n\n")
}

pub fn url_errors(response: &ContentsResponse) -> Vec<&UrlStatus> {
    response
        .statuses
        .iter()
        .filter(|status| status.status == "error")
        .collect()
}

pub fn format_empty_contents_errors(errors: &[&UrlStatus]) -> String {
    let details = errors
        .iter()
        .map(|status| format!("{}: {}", status.id, status.error_tag()))
        .collect::<Vec<_>>()
        .join("; ");
    format!("Error fetching URL(s): {details}")
}

pub fn format_contents(response: &ContentsResponse) -> String {
    if response.results.is_empty() {
        return NO_CONTENT.to_owned();
    }

    let mut lines = Vec::new();
    for result in &response.results {
        lines.push(format!(
            "# {}",
            result.title.as_deref().unwrap_or("(no title)")
        ));
        lines.push(format!("URL: {}", result.url));
        if let Some(published_date) = &result.published_date {
            lines.push(format!(
                "Published: {}",
                published_date.split('T').next().unwrap_or(published_date)
            ));
        }
        if let Some(author) = &result.author {
            lines.push(format!("Author: {author}"));
        }
        lines.push(String::new());
        if let Some(text) = &result.text {
            lines.push(text.clone());
        }
        lines.push(String::new());
    }
    for status in url_errors(response) {
        lines.push(format!(
            "Error fetching {}: {}",
            status.id,
            status.error_tag()
        ));
    }
    lines.join("\n").trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::model::{ContentsResponse, SearchResponse};

    #[test]
    fn formats_search_fixture_byte_for_byte() {
        let response: SearchResponse =
            serde_json::from_str(include_str!("../tests/fixtures/search_response.json")).unwrap();
        assert_eq!(
            format_search(&response),
            include_str!("../tests/fixtures/search_output.txt").trim_end()
        );
    }

    #[test]
    fn formats_contents_fixture_byte_for_byte() {
        let response: ContentsResponse =
            serde_json::from_str(include_str!("../tests/fixtures/contents_response.json")).unwrap();
        assert_eq!(
            format_contents(&response),
            include_str!("../tests/fixtures/contents_output.txt").trim_end()
        );
    }

    #[test]
    fn returns_exact_empty_messages() {
        assert_eq!(format_search(&SearchResponse::default()), NO_SEARCH_RESULTS);
        assert_eq!(format_contents(&ContentsResponse::default()), NO_CONTENT);
    }

    #[test]
    fn formats_empty_results_with_errors() {
        let response: ContentsResponse =
            serde_json::from_str(include_str!("../tests/fixtures/contents_empty_error.json"))
                .unwrap();
        let errors = url_errors(&response);
        assert_eq!(
            format_empty_contents_errors(&errors),
            "Error fetching URL(s): https://missing.test: CRUX_ERROR; https://unknown.test: unknown error"
        );
    }
}
