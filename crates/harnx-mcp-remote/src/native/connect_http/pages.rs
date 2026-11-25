//! Connect forms and non-cacheable, escaped browser responses.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use chrono::{DateTime, NaiveDate, Utc};

use super::gate::{gate, load_flow, Gate};
use super::AppState;

const MAX_FAILURE_MESSAGE_CHARS: usize = 300;

pub(super) async fn connect_page(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let flow = match gate(&state, &headers, load_flow(&state, &id).await) {
        Gate::Pending(flow) => flow,
        Gate::Refuse(response) => return response,
    };
    let config = &state.lifecycle.config;
    let server = escape_html(&config.server_name);
    let action = escape_html(&id);
    let csrf = escape_html(flow.record.csrf_token.expose());
    let mut body = String::new();
    body.push_str(&format!(
        "<h1>Connect {server}</h1>\
         <p>Sign in with your own account. Return here when the connection is complete.</p>"
    ));
    if config.auth_methods.oauth {
        body.push_str(&format!(
            "<p><a href=\"{action}/oauth\">Sign in with {server} (OAuth)</a></p>"
        ));
    }
    if config.auth_methods.token {
        body.push_str(&format!(
            "<form method=\"post\" action=\"{action}/token\">\
                 <input type=\"hidden\" name=\"csrf_token\" value=\"{csrf}\">\
                 <label>API token <input type=\"password\" name=\"token\" autocomplete=\"off\"></label><br>\
                 <label>Expires (optional, YYYY-MM-DD, at most this server's limit) \
                     <input type=\"date\" name=\"expires_at\"></label><br>\
                 <button type=\"submit\">Connect</button>\
             </form>"
        ));
    }
    page(StatusCode::OK, &format!("Connect {server}"), &body)
}

/// Keep failure messages short and free of transport detail that might embed
/// provider responses verbatim.
pub(super) fn sanitize_message(message: &str) -> String {
    let single_line: String = message
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let trimmed = single_line.trim();
    if trimmed.chars().count() > MAX_FAILURE_MESSAGE_CHARS {
        let cut: String = trimmed.chars().take(MAX_FAILURE_MESSAGE_CHARS).collect();
        cut
    } else {
        trimmed.to_string()
    }
}

/// Parse the optional expiry date a user picks when pasting a token (R2): a
/// bare `YYYY-MM-DD` or a full RFC 3339 timestamp.
pub(super) fn parse_expiry(raw: Option<&str>) -> Result<Option<DateTime<Utc>>, &'static str> {
    let Some(raw) = raw else { return Ok(None) };
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    let parsed = if let Ok(date) = NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
        date.and_hms_opt(0, 0, 0).map(|time| time.and_utc())
    } else {
        DateTime::parse_from_rfc3339(raw)
            .ok()
            .map(|value| value.with_timezone(&Utc))
    };
    let Some(parsed) = parsed else {
        return Err("Enter the expiry as YYYY-MM-DD.");
    };
    if parsed <= Utc::now() {
        return Err("The expiry date is in the past.");
    }
    Ok(Some(parsed))
}

/// One inline page: no-store, no external assets, no request data echoed.
pub(super) fn page(status: StatusCode, title: &str, body: &str) -> Response {
    let title = escape_html(title);
    let html = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{title} · harnx</title></head>\
         <body><main><h1>{title}</h1>{body}</main></body></html>"
    );
    (
        status,
        [
            (
                axum::http::header::CACHE_CONTROL,
                axum::http::HeaderValue::from_static("no-store"),
            ),
            (
                axum::http::header::REFERRER_POLICY,
                axum::http::HeaderValue::from_static("no-referrer"),
            ),
        ],
        Html(html),
    )
        .into_response()
}

pub(super) fn escape_html(raw: &str) -> String {
    let mut escaped = String::with_capacity(raw.len());
    for char in raw.chars() {
        match char {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            other => escaped.push(other),
        }
    }
    escaped
}
