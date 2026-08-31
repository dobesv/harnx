use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::Router;
use harnx_healthz::Readiness;
use harnx_mcp_plans_core::server::{PlansServer, ServerMeta, TargetPolicy};
use harnx_mcp_plans_core::{PageToken, Plan, PlanStore, RepoTarget, Target};
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::ServiceExt;
use tokio_util::sync::CancellationToken;

use crate::auth::{GitHubAuth, SystemClock};
use crate::client::GitHubClientFactory;
use crate::config::AppConfig;
use crate::ratelimit::{RateLimitExecutor, TokioSleeper};
use crate::store_github::GitHubPlanStore;

const RETENTION_SCAN_INTERVAL: Duration = Duration::from_secs(60 * 60);
const BASE_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(300);

struct HttpServerLoop {
    store: Arc<GitHubPlanStore>,
    default_repo: Option<RepoTarget>,
    retention_days: u64,
    listener: tokio::net::TcpListener,
    app: Router,
    cancellation: CancellationToken,
}

fn github_server_meta(default_repo: Option<RepoTarget>) -> ServerMeta {
    let target_note = match &default_repo {
        Some(default_repo) => format!(
            " Target repository defaults to {}/{} detected at startup; owner/repo tool args may override it.",
            default_repo.owner, default_repo.repo
        ),
        None => " No default repository was detected at startup; owner and repo tool args are required.".to_string(),
    };
    ServerMeta {
        name: "harnx-mcp-plans-github".into(),
        instructions: format!(
            "GitHub Issues-backed plan/task/note management server using issue bodies and comment front matter.{target_note}"
        )
        .into(),
        target_policy: TargetPolicy::GitHub { default_repo },
    }
}

pub async fn run(config: AppConfig) -> Result<()> {
    // Initialize metrics recorder (idempotent)
    if let Some(ref addr) = config.metrics_addr {
        harnx_metrics::init(&harnx_metrics::MetricsFlags {
            metrics_addr: Some(addr.clone()),
        })?;
    }

    // Initialize healthz listener (opt-in, starts not-ready)
    let readiness = harnx_healthz::init(&harnx_healthz::HealthzFlags {
        healthz_addr: config.healthz_addr.clone(),
    })
    .await?;

    let store = build_store(&config).await?;

    if config.http {
        run_http(store, config, readiness).await
    } else {
        run_stdio(store, config, readiness).await
    }
}

pub async fn build_store(config: &AppConfig) -> Result<Arc<GitHubPlanStore>> {
    let auth = GitHubAuth::new(config.auth.clone()).context("configure GitHub auth")?;
    let ratelimit = Arc::new(RateLimitExecutor::new(
        Arc::new(SystemClock),
        Arc::new(TokioSleeper),
        config.rate_limit.clone(),
    ));
    let client_factory =
        GitHubClientFactory::new(auth, ratelimit).context("build GitHub API client factory")?;
    Ok(Arc::new(GitHubPlanStore::with_config(
        client_factory,
        config.store.clone(),
    )))
}

pub async fn run_retention_pass(
    store: &GitHubPlanStore,
    target: &Target,
    retention_days: u64,
) -> Result<()> {
    if retention_days == 0 {
        return Ok(());
    }

    let cutoff =
        jiff::Timestamp::now() - std::time::Duration::from_secs(retention_days * 24 * 60 * 60);

    let mut page: Option<PageToken> = None;
    loop {
        let response = store.list_plans(target, page.clone()).await?;
        for plan in response.items {
            if plan.updated_at.unwrap_or(plan.created_at) < cutoff {
                close_stale_plan(store, target, &plan).await?;
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

fn default_repo_target(config: &AppConfig) -> Option<RepoTarget> {
    config.default_repo.as_ref().map(|repo| RepoTarget {
        owner: repo.owner.clone(),
        repo: repo.repo.clone(),
    })
}

fn log_startup_target(default_repo: Option<&RepoTarget>) {
    match default_repo {
        Some(default_repo) => eprintln!(
            "harnx-mcp-plans-github: default repository detected: {}/{}",
            default_repo.owner, default_repo.repo
        ),
        None => eprintln!(
            "harnx-mcp-plans-github: no default repository detected; owner/repo are required per tool call"
        ),
    }
}

async fn run_stdio(
    store: Arc<GitHubPlanStore>,
    config: AppConfig,
    readiness: Option<Readiness>,
) -> Result<()> {
    let default_repo = default_repo_target(&config);
    log_startup_target(default_repo.as_ref());

    eprintln!(
        "harnx-mcp-plans-github v{}: starting (label: {}, retention: {} days)",
        env!("CARGO_PKG_VERSION"),
        config.store.plan_label,
        config.retention_days
    );

    let server = PlansServer::with_meta(store.clone(), github_server_meta(default_repo.clone()));
    let transport = rmcp::transport::stdio();
    let service = server.serve(transport).await?;

    // Mark ready once stdio serve loop is active
    if let Some(r) = &readiness {
        r.ready();
    }

    if config.retention_days == 0 {
        eprintln!("[retention] disabled");
        service.waiting().await?;
        return Ok(());
    }

    let mut retention_handle = tokio::spawn(retention_loop(
        store.clone(),
        default_repo.clone(),
        config.retention_days,
    ));
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
                retention_handle = tokio::spawn(retention_loop(store.clone(), default_repo.clone(), config.retention_days));
            }
        }
    }

    Ok(())
}

/// Builds the MCP service for HTTP mode.
fn build_mcp_service(
    store: Arc<GitHubPlanStore>,
    default_repo: Option<RepoTarget>,
    ct: CancellationToken,
) -> StreamableHttpService<PlansServer<GitHubPlanStore>, NeverSessionManager> {
    let server_config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_cancellation_token(ct);
    StreamableHttpService::new(
        move || {
            Ok(PlansServer::with_meta(
                store.clone(),
                github_server_meta(default_repo.clone()),
            ))
        },
        Arc::new(NeverSessionManager::default()),
        server_config,
    )
}

async fn run_http_server_loop(config: HttpServerLoop) -> Result<()> {
    let HttpServerLoop {
        store,
        default_repo,
        retention_days,
        listener,
        app,
        cancellation: ct,
    } = config;
    if retention_days == 0 {
        eprintln!("[retention] disabled");
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { ct.cancelled().await })
            .await?;
        return Ok(());
    }

    let mut retention_handle = tokio::spawn(retention_loop(
        store.clone(),
        default_repo.clone(),
        retention_days,
    ));
    let mut backoff = BASE_BACKOFF;

    let shutdown_ct = ct.clone();
    let server_handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { shutdown_ct.cancelled().await })
            .await
    });
    tokio::pin!(server_handle);

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
                retention_handle = tokio::spawn(retention_loop(store.clone(), default_repo.clone(), retention_days));
            }
        }
    }

    Ok(())
}

async fn run_http(
    store: Arc<GitHubPlanStore>,
    config: AppConfig,
    readiness: Option<Readiness>,
) -> Result<()> {
    let default_repo = default_repo_target(&config);
    log_startup_target(default_repo.as_ref());

    let ct = CancellationToken::new();
    let mcp_service = build_mcp_service(store.clone(), default_repo.clone(), ct.child_token());
    let app = Router::new()
        .nest_service("/mcp", mcp_service)
        .layer(axum::middleware::from_fn(
            harnx_metrics::http_metrics_middleware,
        ));
    let listener = tokio::net::TcpListener::bind((config.host.as_str(), config.port))
        .await
        .with_context(|| {
            format!(
                "harnx-mcp-plans-github: failed to bind {}:{}",
                config.host, config.port
            )
        })?;

    // Mark ready once HTTP listener is bound
    if let Some(r) = &readiness {
        r.ready();
    }

    spawn_shutdown_handler(ct.clone(), readiness);

    eprintln!(
        "harnx-mcp-plans-github v{}: listening on http://{}:{}/mcp (label: {}, retention: {} days)",
        env!("CARGO_PKG_VERSION"),
        config.host,
        config.port,
        config.store.plan_label,
        config.retention_days
    );

    run_http_server_loop(HttpServerLoop {
        store,
        default_repo,
        retention_days: config.retention_days,
        listener,
        app,
        cancellation: ct,
    })
    .await
}

async fn retention_loop(
    store: Arc<GitHubPlanStore>,
    default_repo: Option<RepoTarget>,
    retention_days: u64,
) {
    let Some(default_repo) = default_repo else {
        eprintln!(
            "[retention] disabled because no default GitHub repository was detected at startup"
        );
        return;
    };
    let target = Target::GitHub(default_repo);
    loop {
        if let Err(err) = run_retention_pass(store.as_ref(), &target, retention_days).await {
            eprintln!("[retention] pass failed: {}", sanitize_github_error(err));
        }
        tokio::time::sleep(RETENTION_SCAN_INTERVAL).await;
    }
}

async fn close_stale_plan(store: &GitHubPlanStore, target: &Target, plan: &Plan) -> Result<()> {
    if !store.config_ref().delete_is_close {
        return Ok(());
    }

    let Target::GitHub(repo) = target else {
        return Ok(());
    };
    store
        .client_for(repo)?
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

fn spawn_shutdown_handler(ct: CancellationToken, readiness: Option<Readiness>) {
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

        // Mark not-ready before cancellation
        if let Some(r) = &readiness {
            r.not_ready();
        }

        ct.cancel();
    });
}
