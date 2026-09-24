use std::time::Duration;

use serde_json::Value;

pub const DEFAULT_BASE_URL: &str = "https://api.exa.ai";

#[derive(Debug, PartialEq)]
pub enum RequestOutcome {
    Ok(Value),
    Unauthorized,
    RateLimited,
    HttpStatus(u16),
    Timeout,
    Malformed(String),
    Network(String),
}

pub async fn post_json(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    integration: &str,
    body: &Value,
) -> RequestOutcome {
    let response = match client
        .post(url)
        .header("x-api-key", api_key)
        .header("Content-Type", "application/json")
        .header("x-exa-integration", integration)
        .json(body)
        .timeout(Duration::from_secs(30))
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => return classify_error(error),
    };

    match response.status().as_u16() {
        status if (200..300).contains(&status) => match response.json::<Value>().await {
            Ok(body) => RequestOutcome::Ok(body),
            Err(error) if error.is_decode() => RequestOutcome::Malformed(error.to_string()),
            Err(error) => classify_error(error),
        },
        401 | 403 => RequestOutcome::Unauthorized,
        429 => RequestOutcome::RateLimited,
        status => RequestOutcome::HttpStatus(status),
    }
}

fn classify_error(error: reqwest::Error) -> RequestOutcome {
    if error.is_timeout() {
        RequestOutcome::Timeout
    } else {
        RequestOutcome::Network(error.to_string())
    }
}
