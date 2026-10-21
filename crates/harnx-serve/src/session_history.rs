//! Read-only, round-safe transcript pages. Cursors freeze a raw-log prefix;
//! cached projections are optional and never bypass the route's access check.

mod cache;
mod groups;
mod params;
mod projection;
mod reader;

use crate::session_actor::{SessionKey, SessionRegistry};
pub(crate) use cache::HistoryCache;
use harnx_runtime::config::Config;
use http::StatusCode;
pub(crate) use params::HistoryParams;
use projection::HistoryPage;
use serde_json::{json, Value};

pub(crate) enum HistoryError {
    Missing,
    Invalid(String),
    Storage,
}

impl From<anyhow::Error> for HistoryError {
    fn from(error: anyhow::Error) -> Self {
        log::warn!("Failed to read session history page: {error:#}");
        Self::Storage
    }
}

impl HistoryError {
    pub fn status(&self) -> StatusCode {
        match self {
            Self::Missing => StatusCode::NOT_FOUND,
            Self::Invalid(_) => StatusCode::BAD_REQUEST,
            Self::Storage => StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    pub fn rpc(&self, id: Value, key: &SessionKey) -> Value {
        let (code, message, data) = match self {
            Self::Missing => (
                -32001,
                "session not found",
                Some(json!({"agent":key.agent(),"session":key.session})),
            ),
            Self::Invalid(detail) => (
                -32602,
                "invalid history params",
                Some(json!({"detail": detail})),
            ),
            Self::Storage => (-32003, "session history unavailable", None),
        };
        crate::ag_ui_rpc::json_rpc_error(id, code, message, data)
    }
}

pub(crate) async fn read(
    config: &Config,
    registry: &SessionRegistry,
    key: &SessionKey,
    params: HistoryParams,
) -> Result<HistoryPage, HistoryError> {
    let request = params.parse().map_err(invalid)?;
    reader::read(config, registry, key, request).await
}

fn invalid(error: anyhow::Error) -> HistoryError {
    HistoryError::Invalid(format!("{error:#}"))
}

pub(crate) async fn rpc(
    id: Value,
    params: Option<Value>,
    context: (&Config, &SessionRegistry, SessionKey),
) -> anyhow::Result<crate::ag_ui::AppResponse> {
    let parsed = serde_json::from_value::<HistoryParams>(params.unwrap_or_else(|| json!({})));
    let result = match parsed {
        Ok(params) => read(context.0, context.1, &context.2, params).await,
        Err(error) => Err(HistoryError::Invalid(error.to_string())),
    };
    match result {
        Ok(page) => crate::ag_ui_rpc::json_rpc_response(
            StatusCode::OK,
            json!({"jsonrpc":"2.0","id":id,"result":page}),
        ),
        Err(error) => {
            crate::ag_ui_rpc::json_rpc_response(error.status(), error.rpc(id, &context.2))
        }
    }
}

pub(crate) async fn http(
    query: Option<&str>,
    context: (&Config, &SessionRegistry, SessionKey),
) -> anyhow::Result<crate::ag_ui::AppResponse> {
    let params = query_params(query);
    let result = match params {
        Ok(params) => read(context.0, context.1, &context.2, params).await,
        Err(error) => Err(invalid(error)),
    };
    let mut response = match result {
        Ok(page) => crate::json_response(serde_json::to_value(page)?)?,
        Err(error) => {
            let message = match &error {
                HistoryError::Missing => "session not found",
                HistoryError::Invalid(detail) => detail,
                HistoryError::Storage => "session history unavailable",
            };
            let mut response = crate::ret_err(message);
            *response.status_mut() = error.status();
            response
        }
    };
    response.headers_mut().insert(
        http::header::CACHE_CONTROL,
        http::HeaderValue::from_static("private, no-store"),
    );
    Ok(response)
}

fn query_params(query: Option<&str>) -> anyhow::Result<HistoryParams> {
    let mut params = serde_json::Map::new();
    let url = reqwest::Url::parse(&format!("http://localhost/?{}", query.unwrap_or_default()))?;
    for (key, value) in url.query_pairs() {
        anyhow::ensure!(
            !params.contains_key(key.as_ref()),
            "duplicate history parameter: {key}"
        );
        let value = match key.as_ref() {
            "cursor" => Value::String(value.into_owned()),
            "before_compaction" | "from_seq" | "to_seq" | "limit" => json!(value
                .parse::<u64>()
                .map_err(|_| anyhow::anyhow!("{key} must be an unsigned integer"))?),
            _ => anyhow::bail!("unknown history parameter: {key}"),
        };
        params.insert(key.into_owned(), value);
    }
    Ok(serde_json::from_value(Value::Object(params))?)
}

#[cfg(test)]
mod tests;
