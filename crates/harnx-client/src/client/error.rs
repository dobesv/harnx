//! Keep provider diagnostics typed on both HTTP and in-band stream failures.
use super::*;

pub fn catch_error(data: &Value, status: u16, retry_after: Option<Duration>) -> Result<()> {
    if (200..300).contains(&status) {
        return Ok(());
    }
    debug!("Invalid response, status: {status}, data: {data}");
    Err(provider_error(data, status, retry_after).into())
}

/// An in-band error arrives over HTTP 200, so its transport status isn't useful.
/// Known overflow diagnostics must bypass transient backoff and reach compaction.
pub fn catch_stream_error(data: &Value) -> Result<()> {
    let mut error = provider_error(data, 400, None);
    if !error.is_context_overflow() {
        error.status = 500;
    }
    Err(error.into())
}

fn provider_error(data: &Value, status: u16, retry_after: Option<Duration>) -> LlmError {
    LlmError {
        status,
        message: error_message(data)
            .unwrap_or_else(|| format!("Invalid response data: {data} (status: {status})")),
        retry_after,
    }
}

fn error_message(data: &Value) -> Option<String> {
    for error in [&data["error"], &data["errors"][0], &data[0]["error"]] {
        if let Some(message) = object_message(error) {
            return Some(message);
        }
    }
    if let (Some(detail), Some(code)) = (data["detail"].as_str(), data["status"].as_i64()) {
        return Some(format!("{detail} (status: {code})"));
    }
    data["error"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| object_message(data))
}

fn object_message(error: &Value) -> Option<String> {
    let mut message = error["message"].as_str()?.to_owned();
    // Keep code even when type exists: OpenAI often uses a generic type with a
    // specific context_length_exceeded code and an otherwise unhelpful message.
    for key in ["type", "code", "status"] {
        if let Some(value) = error.get(key).filter(|value| !value.is_null()) {
            let value = value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string());
            message.push_str(&format!(" ({key}: {value})"));
        }
    }
    Some(message)
}

#[cfg(test)]
mod tests;
