use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::Router;
use harnx_mcp_plans_core::server::{PlansServer, ServerMeta};
use harnx_mcp_plans_core::{PageToken, Plan, PlanStore};
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::ServiceExt;
use tokio_util::sync::CancellationToken;

use crate::auth::{GitHubAuth, SystemClock};
use crate::config::AppConfig;
use crate::ratelimit::{RateLimitExecutor, TokioSleeper};
use crate::store_github::GitHubPlanStore;

const LABEL_COLOR: &str = "5319e7";
const LABEL_DESCRIPTION: &str = "harnx plans root issue";
const RETENTION_SCAN_INTERVAL: Duration = Duration::from_secs(60 * 60);
const BASE_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(300);
const GITHUB_SERVER_META: ServerMeta = ServerMeta::new(
    "harnx-mcp-plans-github",
    "GitHub Issues-backed plan/task/note management server using issue bodies and comment front matter",
);

pub async fn run(config: AppConfig) -> Result<()> {
    let store = build_store(&config).await?;
    validate_startup(store.as_ref(), &config.store.plan_label).await?;

    if config.http {
        run_http(store, config).await
    } else {
        run_stdio(store, config).await
    }
}

pub async fn build_store(config: &AppConfig) -> Result<Arc<GitHubPlanStore>> {
    let auth = GitHubAuth::new(config.auth.clone()).context("configure GitHub auth")?;
    let ratelimit = Arc::new(RateLimitExecutor::new(
        Arc::new(SystemClock),
        Arc::new(TokioSleeper),
        config.rate_limit.clone(),
    ));
    let client = crate::client::GitHubClient::with_ratelimit(
        auth,
        config.auth.repo.owner.clone(),
        config.auth.repo.repo.clone(),
        ratelimit,
    )
    .await
    .context("build GitHub API client")?;
    Ok(Arc::new(GitHubPlanStore::with_config(
        client,
        config.store.clone(),
    )))
}

pub async fn validate_startup(store: &GitHubPlanStore, plan_label: &str) -> Result<()> {
    store
        .client_ref()
        .get_repository()
        .await
        .map_err(sanitize_github_error)
        .context("startup validation failed: cannot access configured repository")?;
    store
        .client_ref()
        .ensure_label(plan_label, LABEL_COLOR, LABEL_DESCRIPTION)
        .await
        .map_err(sanitize_github_error)
        .with_context(|| {
            format!("startup validation failed: cannot ensure label '{plan_label}'")
        })?;
    Ok(())
}

pub async fn run_retention_pass(store: &GitHubPlanStore, retention_days: u64) -> Result<()> {
    if retention_days == 0 {
        return Ok(());
    }

    let cutoff =
        jiff::Timestamp::now() - std::time::Duration::from_secs(retention_days * 24 * 60 * 60);

    let mut page: Option<PageToken> = None;
    loop {
        let response = store.list_plans(page.clone()).await?;
        for plan in response.items {
            if plan.updated_at.unwrap_or(plan.created_at) < cutoff {
                close_stale_plan(store, &plan).await?;
            }
        }
        if response.next.is_none() {
            break;
        }
        page = response.next;
    }

    Ok(())
}

pub fn sanitize_github_error(err: anyhow::Error) -> anyhow::Error {
    anyhow::anyhow!(redact_bearer_tokens(&err.to_string()))
}

fn redact_bearer_tokens(input: &str) -> String {
    regex::Regex::new(r"(?i)bearer\s+[a-z0-9_\-.]+")
        .expect("valid regex")
        .replace_all(input, "Bearer [REDACTED]")
        .into_owned()
}

async fn run_stdio(store: Arc<GitHubPlanStore>, config: AppConfig) -> Result<()> {
    eprintln!(
        "harnx-mcp-plans-github v{}: starting (repo: {}/{}, label: {}, retention: {} days)",
        env!("CARGO_PKG_VERSION"),
        config.auth.repo.owner,
        config.auth.repo.repo,
        config.store.plan_label,
        config.retention_days
    );

    let server = PlansServer::with_meta(store.clone(), GITHUB_SERVER_META);
    let transport = rmcp::transport::stdio();
    let service = server.serve(transport).await?;

    if config.retention_days == 0 {
        eprintln!("[retention] disabled");
        service.waiting().await?;
        return Ok(());
    }

    let mut retention_handle = tokio::spawn(retention_loop(store.clone(), config.retention_days));
    let service_handle = tokio::spawn(async move { service.waiting().await });
    tokio::pin!(service_handle);
    let mut backoff = BASE_BACKOFF;

    loop {
        tokio::select! {
            result = &mut *service_handle => {
                retention_handle.abort();
                result??;
                break;
            }
            result = &mut retention_handle => {
                match result {
                    Err(err) => {
                        eprintln!("[retention] task failed: {err}");
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(MAX_BACKOFF);
                    }
                    Ok(()) => backoff = BASE_BACKOFF,
                }
                retention_handle = tokio::spawn(retention_loop(store.clone(), config.retention_days));
            }
        }
    }

    Ok(())
}

async fn run_http(store: Arc<GitHubPlanStore>, config: AppConfig) -> Result<()> {
    let ct = CancellationToken::new();
    let factory_store = store.clone();
    let server_config = StreamableHttpServerConfig::default()
        .with_stateful_mode(false)
        .with_json_response(true)
        .with_cancellation_token(ct.child_token());
    let mcp_service = StreamableHttpService::new(
        move || {
            Ok(PlansServer::with_meta(
                factory_store.clone(),
                GITHUB_SERVER_META,
            ))
        },
        Arc::new(NeverSessionManager::default()),
        server_config,
    );
    let app = Router::new().nest_service("/mcp", mcp_service);
    let listener = tokio::net::TcpListener::bind((config.host.as_str(), config.port))
        .await
        .with_context(|| {
            format!(
                "harnx-mcp-plans-github: failed to bind {}:{}",
                config.host, config.port
            )
        })?;

    spawn_shutdown_handler(ct.clone());

    eprintln!(
        "harnx-mcp-plans-github v{}: listening on http://{}:{}/mcp (repo: {}/{}, label: {}, retention: {} days)",
        env!("CARGO_PKG_VERSION"),
        config.host,
        config.port,
        config.auth.repo.owner,
        config.auth.repo.repo,
        config.store.plan_label,
        config.retention_days
    );

    let shutdown_ct = ct.clone();
    let server_handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { shutdown_ct.cancelled().await })
            .await
    });
    tokio::pin!(server_handle);

    if config.retention_days == 0 {
        eprintln!("[retention] disabled");
        (&mut server_handle).await??;
        return Ok(());
    }

    let mut retention_handle = tokio::spawn(retention_loop(store.clone(), config.retention_days));
    let mut backoff = BASE_BACKOFF;

    loop {
        tokio::select! {
            result = &mut *server_handle => {
                retention_handle.abort();
                result??;
                break;
            }
            result = &mut retention_handle => {
                match result {
                    Err(err) => {
                        eprintln!("[retention] task failed: {err}");
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(MAX_BACKOFF);
                    }
                    Ok(()) => backoff = BASE_BACKOFF,
                }
                retention_handle = tokio::spawn(retention_loop(store.clone(), config.retention_days));
            }
        }
    }

    Ok(())
}

async fn retention_loop(store: Arc<GitHubPlanStore>, retention_days: u64) {
    loop {
        if let Err(err) = run_retention_pass(store.as_ref(), retention_days).await {
            eprintln!("[retention] pass failed: {}", sanitize_github_error(err));
        }
        tokio::time::sleep(RETENTION_SCAN_INTERVAL).await;
    }
}

async fn close_stale_plan(store: &GitHubPlanStore, plan: &Plan) -> Result<()> {
    if !store.config_ref().delete_is_close {
        return Ok(());
    }

    store
        .client_ref()
        .close_issue(parse_issue_number(&plan.id)?)
        .await
        .map_err(sanitize_github_error)
        .with_context(|| format!("close stale plan {}", plan.id))?;
    Ok(())
}

fn parse_issue_number(value: &str) -> Result<u64> {
    value
        .parse::<u64>()
        .with_context(|| format!("invalid issue number: {value}"))
}

fn spawn_shutdown_handler(ct: CancellationToken) {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(mut sigterm) => {
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => {}
                        _ = sigterm.recv() => {}
                    }
                }
                Err(err) => {
                    eprintln!(
                        "harnx-mcp-plans-github: failed to install SIGTERM handler ({err}); falling back to Ctrl-C only"
                    );
                    let _ = tokio::signal::ctrl_c().await;
                }
            }
        }

        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }

        ct.cancel();
    });
}
