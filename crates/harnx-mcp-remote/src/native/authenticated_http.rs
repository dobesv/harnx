//! Refresh at the HTTP boundary, including requests on an existing MCP session.
//! Only an HTTP 401 (rejected before execution) permits one POST retry.

use std::{collections::HashMap, sync::Arc};

use futures::stream::BoxStream;
use reqwest::header::{HeaderName, HeaderValue, AUTHORIZATION};
use rmcp::{
    model::ClientJsonRpcMessage,
    transport::streamable_http_client::{
        SseError, StreamableHttpClient, StreamableHttpClientTransportConfig, StreamableHttpError,
        StreamableHttpPostResponse,
    },
};
use tracing::instrument::WithSubscriber;

use super::pool::UpstreamAuth;

type Headers = HashMap<HeaderName, HeaderValue>;
type HttpResult<T> = Result<T, StreamableHttpError<SafeHttpError>>;
type Events = BoxStream<'static, Result<sse_stream::Sse, SseError>>;

// rmcp 3.8 keeps its 16 MiB default private. Explicit transport limits still win.
const DEFAULT_MAX_SSE_EVENT_SIZE: usize = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct SafeHttpError(pub String);

#[derive(Clone)]
pub(crate) struct AuthenticatedHttp {
    pub http: reqwest::Client,
    pub auth: Arc<dyn UpstreamAuth>,
}

impl AuthenticatedHttp {
    async fn headers(&self, mut headers: Headers) -> HttpResult<Headers> {
        self.auth
            .ensure_fresh()
            .await
            .map_err(|error| StreamableHttpError::Client(SafeHttpError(error.to_string())))?;
        let mut config = StreamableHttpClientTransportConfig::with_uri("");
        self.auth.apply(&mut config);
        headers.extend(config.custom_headers);
        Ok(headers)
    }

    fn auth_error(&self) -> StreamableHttpError<SafeHttpError> {
        StreamableHttpError::Client(SafeHttpError(self.auth.reconnect_message()))
    }

    async fn rejected(&self, headers: &Headers) -> HttpResult<Headers> {
        self.auth
            .refresh_after_rejection(headers.get(&AUTHORIZATION))
            .await
            .map_err(|error| StreamableHttpError::Client(SafeHttpError(error.to_string())))?;
        self.headers(headers.clone()).await
    }

    fn rejected_before_execution(error: &StreamableHttpError<reqwest::Error>) -> bool {
        match error {
            StreamableHttpError::AuthRequired(_) => true,
            StreamableHttpError::HttpStatus(error) => {
                error.status == reqwest::StatusCode::UNAUTHORIZED.as_u16()
            }
            // rmcp 3.8 preserves only 429/5xx as HttpStatus. Its challenge-less
            // 401 path builds this exact status prefix before the response body.
            // Don't search arbitrary error text: that could replay an executed call.
            StreamableHttpError::UnexpectedServerResponse(message) => {
                message.starts_with("HTTP 401 Unauthorized:")
            }
            // Some providers omit WWW-Authenticate on their 401 response.
            StreamableHttpError::Client(error) => {
                error.status() == Some(reqwest::StatusCode::UNAUTHORIZED)
            }
            _ => false,
        }
    }

    // qual:allow(iosp) reason: "Classify errors at the boundary that removes provider diagnostics before rmcp can log them."
    fn safe_error(
        &self,
        error: StreamableHttpError<reqwest::Error>,
    ) -> StreamableHttpError<SafeHttpError> {
        match error {
            error if Self::rejected_before_execution(&error) => self.auth_error(),
            StreamableHttpError::ServerDoesNotSupportSse => {
                StreamableHttpError::ServerDoesNotSupportSse
            }
            StreamableHttpError::ServerDoesNotSupportDeleteSession => {
                StreamableHttpError::ServerDoesNotSupportDeleteSession
            }
            StreamableHttpError::SessionExpired => StreamableHttpError::SessionExpired,
            StreamableHttpError::AuthRequired(_) => self.auth_error(),
            StreamableHttpError::InsufficientScope(_) => {
                StreamableHttpError::Client(SafeHttpError(format!(
                    "{}; missing required scope (insufficient_scope)",
                    self.auth.reconnect_message()
                )))
            }
            // Provider bodies, authentication challenges and URLs are untrusted.
            // rmcp errors can contain all three; never let them reach its worker logs.
            _ => StreamableHttpError::Client(SafeHttpError("upstream HTTP request failed".into())),
        }
    }
}

impl StreamableHttpClient for AuthenticatedHttp {
    type Error = SafeHttpError;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: Headers,
    ) -> HttpResult<StreamableHttpPostResponse> {
        self.post_message_with_max_sse_event_size(
            uri,
            message,
            session_id,
            auth_header,
            custom_headers,
            DEFAULT_MAX_SSE_EVENT_SIZE,
        )
        .await
    }

    // qual:allow(iosp) reason: "Keep the only POST retry next to its pre-execution rejection guard so executed requests cannot be replayed."
    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        _auth_header: Option<String>,
        custom_headers: Headers,
        limit: usize,
    ) -> HttpResult<StreamableHttpPostResponse> {
        let headers = self.headers(custom_headers).await?;
        let result = self
            .http
            .post_message_with_max_sse_event_size(
                uri.clone(),
                message.clone(),
                session_id.clone(),
                None,
                headers.clone(),
                limit,
            )
            .with_subscriber(tracing::subscriber::NoSubscriber::default())
            .await;
        let result = match result {
            Err(error) if self.auth.refreshable() && Self::rejected_before_execution(&error) => {
                let headers = self.rejected(&headers).await?;
                self.http
                    .post_message_with_max_sse_event_size(
                        uri, message, session_id, None, headers, limit,
                    )
                    .with_subscriber(tracing::subscriber::NoSubscriber::default())
                    .await
            }
            other => other,
        };
        result.map_err(|error| self.safe_error(error))
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        _auth_header: Option<String>,
        custom_headers: Headers,
    ) -> HttpResult<()> {
        let headers = self.headers(custom_headers).await?;
        self.http
            .delete_session(uri, session_id, None, headers)
            .with_subscriber(tracing::subscriber::NoSubscriber::default())
            .await
            .map_err(|error| self.safe_error(error))
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: Headers,
    ) -> HttpResult<Events> {
        self.get_stream_with_max_sse_event_size(
            uri,
            session_id,
            last_event_id,
            auth_header,
            custom_headers,
            DEFAULT_MAX_SSE_EVENT_SIZE,
        )
        .await
    }

    // qual:allow(iosp) reason: "A rejected SSE reconnect may refresh once; keep that guard and retry in one auditable boundary."
    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        _auth_header: Option<String>,
        custom_headers: Headers,
        limit: usize,
    ) -> HttpResult<Events> {
        let headers = self.headers(custom_headers).await?;
        let result = self
            .http
            .get_stream_with_max_sse_event_size(
                uri.clone(),
                session_id.clone(),
                last_event_id.clone(),
                None,
                headers.clone(),
                limit,
            )
            .with_subscriber(tracing::subscriber::NoSubscriber::default())
            .await;
        let result = match result {
            Err(error) if self.auth.refreshable() && Self::rejected_before_execution(&error) => {
                let headers = self.rejected(&headers).await?;
                self.http
                    .get_stream_with_max_sse_event_size(
                        uri,
                        session_id,
                        last_event_id,
                        None,
                        headers,
                        limit,
                    )
                    .with_subscriber(tracing::subscriber::NoSubscriber::default())
                    .await
            }
            other => other,
        };
        result.map_err(|error| self.safe_error(error))
    }
}

#[cfg(test)]
mod tests;
