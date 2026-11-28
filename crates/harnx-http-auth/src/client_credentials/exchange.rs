//! The fixed client-credentials grant, without cache or transport policy.

use std::{path::Path, time::Duration};

use anyhow::{anyhow, bail, Result};
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use tokio::time::{timeout_at, Instant};
use tracing::instrument::WithSubscriber;
use url::Url;

use crate::oauth::{read_client_secret_file, MAX_RESPONSE_BODY_BYTES};

use super::token::{self, form_body, parse_token_response, status_error, CachedToken};

/// Absolute token-only deadline. Never applies to the resource client.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) struct Grant {
    endpoint: Url,
    client_id: String,
    client_secret: String,
    scopes: Vec<String>,
    http: reqwest::Client,
}

impl Grant {
    pub(super) fn new(
        endpoint: Url,
        client_id: String,
        client_secret_file: &Path,
        scopes: Vec<String>,
        http: reqwest::Client,
    ) -> Result<Self> {
        let client_secret = read_client_secret_file(client_secret_file)?;
        Ok(Self {
            endpoint,
            client_id,
            client_secret,
            scopes,
            http,
        })
    }

    pub(super) fn client_secret(&self) -> &str {
        &self.client_secret
    }

    pub(super) async fn exchange(&self) -> Result<CachedToken> {
        // One absolute token-only deadline covers headers and body. Starting a
        // fresh timeout after headers would let a stalled response occupy the
        // single-flight slot indefinitely.
        let deadline = exchange_deadline();
        let response = self.send_form(deadline).await?;
        let body = read_response(response, deadline).await?;
        parse_token_response(&body)
    }

    async fn send_form(&self, deadline: Instant) -> Result<reqwest::Response> {
        let body = form_body(&self.client_id, &self.client_secret, &self.scopes);
        timeout_at(
            deadline,
            self.http
                .post(self.endpoint.as_str())
                .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(ACCEPT, "application/json")
                .body(body)
                .send()
                .with_subscriber(tracing::subscriber::NoSubscriber::default()),
        )
        .await
        .map_err(|_| anyhow!("token exchange timed out"))?
        // reqwest errors carry the URL, so they become static text.
        .map_err(|_| anyhow!("token endpoint request failed"))
    }
}

fn exchange_deadline() -> Instant {
    Instant::now() + EXCHANGE_TIMEOUT
}

async fn read_response(mut response: reqwest::Response, deadline: Instant) -> Result<Vec<u8>> {
    let status = response.status();
    if status.is_redirection() {
        bail!(
            "{}",
            token::status_reason(status, "redirects are not allowed")
        );
    }
    let body = match timeout_at(
        deadline,
        token::read_token_body(&mut response, MAX_RESPONSE_BODY_BYTES),
    )
    .await
    {
        Ok(Ok(body)) => body,
        Ok(Err(error)) => bail!("{}", token::body_read_error(status, error)),
        Err(_) => bail!("{}", token::body_timeout_error(status)),
    };
    if !status.is_success() {
        bail!("{}", status_error(status, &body));
    }
    Ok(body)
}
