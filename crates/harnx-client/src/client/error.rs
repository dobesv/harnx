//! Keep provider diagnostics typed on both HTTP and in-band stream failures.
use super::*;
use harnx_core::error::ModelFallbackReason;

pub fn catch_error(data: &Value, status: u16, retry_after: Option<Duration>) -> Result<()> {
    if (200..300).contains(&status) {
        return Ok(());
    }
    debug!("Invalid response, status: {status}, data: {data}");
    Err(classified_error(
        data,
        provider_error(data, status, retry_after),
    ))
}

/// An in-band error arrives over HTTP 200, so its transport status isn't useful.
/// Known overflow diagnostics must bypass transient backoff and reach compaction.
pub fn catch_stream_error(data: &Value) -> Result<()> {
    let mut error = provider_error(data, 400, None);
    if !error.is_context_overflow() && !model_unavailable(data) {
        error.status = 500;
    }
    Err(classified_error(data, error))
}

fn classified_error(data: &Value, error: LlmError) -> anyhow::Error {
    let unavailable = model_unavailable(data);
    let error = anyhow::Error::new(error);
    if unavailable {
        error.context(ModelFallbackReason::Unavailable)
    } else {
        error
    }
}

/// Do not infer model availability from HTTP status alone: a 404 can be a bad URL,
/// and generic invalid_request_error / INVALID_ARGUMENT codes include real bugs.
fn model_unavailable(data: &Value) -> bool {
    diagnostics(data).any(unavailable_diagnostic)
}

fn diagnostics(data: &Value) -> impl Iterator<Item = &Value> {
    nested_diagnostics(data).chain(std::iter::once(data))
}

fn nested_diagnostics(data: &Value) -> impl Iterator<Item = &Value> {
    std::iter::once(&data["error"])
        .chain(data["errors"].as_array().into_iter().flatten())
        .chain(
            data.as_array()
                .into_iter()
                .flatten()
                .map(|entry| entry.get("error").unwrap_or(entry)),
        )
}

fn unavailable_diagnostic(error: &Value) -> bool {
    let code = error["code"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| error["code"].as_u64().map(|code| code.to_string()));
    if matches!(
        code.as_deref(),
        Some("model_not_found" | "model_not_available" | "model_access_denied")
    ) {
        return true;
    }
    let message = error["message"].as_str().unwrap_or_default();
    // Numeric codes aren't globally namespaced. Require the accompanying Z.AI
    // diagnostic too, so another provider's 1211 cannot mask a malformed request.
    if zai_model_unavailable(code.as_deref(), message) {
        return true;
    }
    missing_model_resource(error, message)
}

fn missing_model_resource(error: &Value, message: &str) -> bool {
    // Anthropic identifies the missing resource in the message; Google names
    // both the resource and its unsupported API version/method.
    (error["type"] == "not_found_error" && message.starts_with("model: "))
        || (error["status"] == "NOT_FOUND"
            && message.starts_with("models/")
            && message.contains(" is not found"))
        // Bedrock uses a generic ValidationException for an invalid model ID.
        || message == "The provided model identifier is invalid."
}

fn zai_model_unavailable(code: Option<&str>, message: &str) -> bool {
    (code == Some("1211") && message.to_ascii_lowercase().starts_with("unknown model"))
        || (code == Some("1311")
            && message
                .starts_with("Your current subscription plan does not yet include access to "))
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
    if let Some(message) = diagnostics(data)
        .filter(|error| unavailable_diagnostic(error))
        .find_map(object_message)
    {
        return Some(message);
    }
    for error in nested_diagnostics(data) {
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
