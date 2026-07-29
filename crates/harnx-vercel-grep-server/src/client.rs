use std::time::Duration;

use serde_json::Value;

use crate::server::GrepQueryParams;

pub const DEFAULT_SEARCH_URL: &str = "https://grep.app/api/search";

#[derive(Debug, PartialEq)]
pub enum SearchOutcome {
    Ok(Value),
    NotFound,
    RateLimited,
    HttpStatus(u16),
    Timeout,
    Malformed(String),
    Network(String),
}

pub async fn search(
    client: &reqwest::Client,
    base_url: &str,
    params: &GrepQueryParams,
) -> SearchOutcome {
    let mut query = vec![("q", params.query.trim())];
    if let Some(language) = &params.language {
        query.push(("f.lang", language.trim()));
    }
    if let Some(repo) = &params.repo {
        query.push(("f.repo", repo.trim()));
    }
    if let Some(path) = &params.path {
        query.push(("f.path", path.trim()));
    }

    let response = match client
        .get(base_url)
        .query(&query)
        .timeout(Duration::from_secs(30))
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => return classify_error(error),
    };

    match response.status().as_u16() {
        200 => match response.json::<Value>().await {
            Ok(body) => SearchOutcome::Ok(body),
            Err(error) if error.is_decode() => SearchOutcome::Malformed(error.to_string()),
            Err(error) => classify_error(error),
        },
        429 => SearchOutcome::RateLimited,
        404 => SearchOutcome::NotFound,
        status => SearchOutcome::HttpStatus(status),
    }
}

fn classify_error(error: reqwest::Error) -> SearchOutcome {
    if error.is_timeout() {
        SearchOutcome::Timeout
    } else {
        SearchOutcome::Network(error.to_string())
    }
}
