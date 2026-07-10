use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
#[cfg(test)]
use chrono::{DateTime, Utc};
use harnx_mcp_plans_core::StoreError;
use reqwest::{Method, Response, StatusCode};

use crate::auth::Clock;

const DEFAULT_MAX_WAIT_SECS: u64 = 30;
const DEFAULT_MAX_TRANSIENT_RETRIES: usize = 3;
const BASE_TRANSIENT_BACKOFF_MILLIS: u64 = 250;

pub trait Sleeper: Send + Sync + fmt::Debug {
    fn sleep<'a>(&'a self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
}

#[derive(Debug, Clone, Default)]
pub struct TokioSleeper;

impl Sleeper for TokioSleeper {
    fn sleep<'a>(&'a self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            tokio::time::sleep(duration).await;
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitConfig {
    pub max_wait_secs: u64,
    pub max_transient_retries: usize,
    pub base_transient_backoff: Duration,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            max_wait_secs: std::env::var("GITHUB_MAX_WAIT_SECS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(DEFAULT_MAX_WAIT_SECS),
            max_transient_retries: DEFAULT_MAX_TRANSIENT_RETRIES,
            base_transient_backoff: Duration::from_millis(BASE_TRANSIENT_BACKOFF_MILLIS),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RateLimitExecutor {
    clock: Arc<dyn Clock>,
    sleeper: Arc<dyn Sleeper>,
    config: RateLimitConfig,
}

#[derive(Debug, Clone)]
pub struct RequestContext {
    pub method: Method,
    pub url: String,
}

impl RequestContext {
    pub fn new(method: Method, url: impl Into<String>) -> Self {
        Self {
            method,
            url: url.into(),
        }
    }
}

#[derive(Debug)]
pub enum AttemptOutcome<T> {
    Success(T),
    Response(Response),
}

impl RateLimitExecutor {
    pub fn new(clock: Arc<dyn Clock>, sleeper: Arc<dyn Sleeper>, config: RateLimitConfig) -> Self {
        Self {
            clock,
            sleeper,
            config,
        }
    }

    pub fn config(&self) -> &RateLimitConfig {
        &self.config
    }

    pub async fn run<T, F, Fut>(&self, context: RequestContext, mut operation: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<AttemptOutcome<T>>>,
    {
        let mut transient_retries = 0usize;
        loop {
            match operation().await? {
                AttemptOutcome::Success(value) => return Ok(value),
                AttemptOutcome::Response(response) => {
                    let status = response.status();
                    if let Some(wait_secs) = retry_after_secs(&response, self.clock.as_ref()) {
                        self.sleep_or_rate_limited(wait_secs).await?;
                        continue;
                    }
                    if is_transient(status) && transient_retries < self.config.max_transient_retries
                    {
                        let wait = transient_backoff(
                            self.config.base_transient_backoff,
                            transient_retries,
                        );
                        transient_retries += 1;
                        self.sleep_or_rate_limited(wait.as_secs().max(1)).await?;
                        continue;
                    }
                    let message = response_error_message(&context, status);
                    bail!(message);
                }
            }
        }
    }

    async fn sleep_or_rate_limited(&self, wait_secs: u64) -> Result<()> {
        if wait_secs > self.config.max_wait_secs {
            return Err(StoreError::RateLimited {
                retry_after_secs: wait_secs,
            }
            .into());
        }
        self.sleeper.sleep(Duration::from_secs(wait_secs)).await;
        Ok(())
    }
}

pub async fn send_rate_limited<F, Fut>(
    executor: &RateLimitExecutor,
    context: RequestContext,
    mut send: F,
) -> Result<Response>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Response>>,
{
    executor
        .run(context, || {
            let future = send();
            async move {
                let response = future.await.context("send GitHub request")?;
                if response.status().is_success() {
                    Ok(AttemptOutcome::Success(response))
                } else {
                    Ok(AttemptOutcome::Response(response))
                }
            }
        })
        .await
}

fn retry_after_secs(response: &Response, clock: &dyn Clock) -> Option<u64> {
    let status = response.status();
    if !matches!(
        status,
        StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS
    ) {
        return None;
    }
    if let Some(value) = response.headers().get("retry-after") {
        let parsed = value.to_str().ok()?.parse::<u64>().ok()?;
        return Some(parsed.max(1));
    }
    let remaining = response
        .headers()
        .get("x-ratelimit-remaining")
        .and_then(|value| value.to_str().ok())?;
    if remaining != "0" {
        return None;
    }
    let reset = response
        .headers()
        .get("x-ratelimit-reset")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<i64>().ok())?;
    let now = clock.now().timestamp();
    let wait = reset.saturating_sub(now);
    Some(wait.max(1) as u64)
}

fn transient_backoff(base: Duration, retry: usize) -> Duration {
    let factor = 1u32 << retry.min(10);
    base.saturating_mul(factor)
}

fn is_transient(status: StatusCode) -> bool {
    status.is_server_error()
}

fn response_error_message(context: &RequestContext, status: StatusCode) -> String {
    format!(
        "GitHub request failed: {} {} -> {}",
        context.method,
        sanitize_url(&context.url),
        status
    )
}

fn sanitize_url(url: &str) -> String {
    reqwest::Url::parse(url)
        .map(|parsed| {
            let mut clean = format!("{}://{}", parsed.scheme(), parsed.host_str().unwrap_or(""));
            clean.push_str(parsed.path());
            clean
        })
        .unwrap_or_else(|_| "<invalid-url>".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::SystemClock;
    use chrono::TimeZone;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    #[derive(Debug, Default)]
    struct RecordingSleeper {
        calls: Mutex<Vec<Duration>>,
    }

    impl RecordingSleeper {
        fn recorded(&self) -> Vec<Duration> {
            self.calls.lock().expect("sleeper poisoned").clone()
        }
    }

    impl Sleeper for RecordingSleeper {
        fn sleep<'a>(
            &'a self,
            duration: Duration,
        ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
            Box::pin(async move {
                self.calls.lock().expect("sleeper poisoned").push(duration);
            })
        }
    }

    #[derive(Debug)]
    struct FixedClock(DateTime<Utc>);

    impl Clock for FixedClock {
        fn now(&self) -> DateTime<Utc> {
            self.0
        }
    }

    #[derive(Debug)]
    struct SequenceResponder {
        hits: AtomicUsize,
        templates: Vec<ResponseTemplate>,
    }

    impl SequenceResponder {
        fn new(templates: Vec<ResponseTemplate>) -> Self {
            Self {
                hits: AtomicUsize::new(0),
                templates,
            }
        }
    }

    impl Respond for SequenceResponder {
        fn respond(&self, _request: &Request) -> ResponseTemplate {
            let idx = self.hits.fetch_add(1, Ordering::SeqCst);
            self.templates
                .get(idx)
                .cloned()
                .unwrap_or_else(|| self.templates.last().expect("templates").clone())
        }
    }

    #[tokio::test]
    async fn retries_on_retry_after_then_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/retry-after"))
            .respond_with(SequenceResponder::new(vec![
                ResponseTemplate::new(429).insert_header("retry-after", "1"),
                ResponseTemplate::new(200).set_body_string("ok"),
            ]))
            .mount(&server)
            .await;

        let sleeper = Arc::new(RecordingSleeper::default());
        let executor = RateLimitExecutor::new(
            Arc::new(SystemClock),
            sleeper.clone(),
            RateLimitConfig::default(),
        );
        let client = reqwest::Client::new();
        let url = format!("{}/retry-after", server.uri());

        let response = send_rate_limited(&executor, RequestContext::new(Method::GET, &url), || {
            let client = client.clone();
            let url = url.clone();
            async move { client.get(url).send().await.map_err(Into::into) }
        })
        .await
        .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(sleeper.recorded(), vec![Duration::from_secs(1)]);
    }

    #[tokio::test]
    async fn retries_on_reset_header_then_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/reset"))
            .respond_with(SequenceResponder::new(vec![
                ResponseTemplate::new(403)
                    .insert_header("x-ratelimit-remaining", "0")
                    .insert_header("x-ratelimit-reset", "1700000002"),
                ResponseTemplate::new(200).set_body_string("ok"),
            ]))
            .mount(&server)
            .await;

        let sleeper = Arc::new(RecordingSleeper::default());
        let clock = Arc::new(FixedClock(
            Utc.timestamp_opt(1_700_000_000, 0).single().expect("ts"),
        ));
        let executor = RateLimitExecutor::new(clock, sleeper.clone(), RateLimitConfig::default());
        let client = reqwest::Client::new();
        let url = format!("{}/reset", server.uri());

        let response = send_rate_limited(&executor, RequestContext::new(Method::GET, &url), || {
            let client = client.clone();
            let url = url.clone();
            async move { client.get(url).send().await.map_err(Into::into) }
        })
        .await
        .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(sleeper.recorded(), vec![Duration::from_secs(2)]);
    }

    #[tokio::test]
    async fn returns_rate_limited_when_wait_exceeds_threshold() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/too-long"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "31"))
            .mount(&server)
            .await;

        let sleeper = Arc::new(RecordingSleeper::default());
        let executor = RateLimitExecutor::new(
            Arc::new(SystemClock),
            sleeper.clone(),
            RateLimitConfig {
                max_wait_secs: 30,
                max_transient_retries: DEFAULT_MAX_TRANSIENT_RETRIES,
                base_transient_backoff: Duration::from_millis(BASE_TRANSIENT_BACKOFF_MILLIS),
            },
        );
        let client = reqwest::Client::new();
        let url = format!("{}/too-long", server.uri());

        let error = send_rate_limited(&executor, RequestContext::new(Method::GET, &url), || {
            let client = client.clone();
            let url = url.clone();
            async move { client.get(url).send().await.map_err(Into::into) }
        })
        .await
        .expect_err("rate limited");

        let store_error = error.downcast::<StoreError>().expect("store error");
        match store_error {
            StoreError::RateLimited { retry_after_secs } => assert_eq!(retry_after_secs, 31),
            other => panic!("expected rate limited error, got {other}"),
        }
        assert!(sleeper.recorded().is_empty());
    }
}
