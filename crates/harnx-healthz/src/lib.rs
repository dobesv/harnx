//! Opt-in readiness endpoint for harnx services and tool servers.

use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use anyhow::Context;
use axum::{extract::State, http::StatusCode, routing::get, Router};
use clap::Args;

/// Command-line configuration for the optional healthz listener.
#[derive(Args, Clone, Debug, Default, PartialEq, Eq)]
pub struct HealthzFlags {
    /// Address for the readiness listener.
    #[arg(
        long,
        env = "HARNX_HEALTHZ_ADDR",
        value_name = "ADDR",
        help = "Serve readiness checks at http://ADDR/healthz. Blank host binds 0.0.0.0, e.g. :8457. Unset disables."
    )]
    pub healthz_addr: Option<String>,
}

/// Extract a healthz listener address from command-line arguments.
///
/// Accepts both `--healthz-addr ADDR` and `--healthz-addr=ADDR`.
pub fn healthz_addr_from_args<I: IntoIterator<Item = String>>(args: I) -> Option<String> {
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--healthz-addr" {
            return args.next();
        }
        if let Some(addr) = arg.strip_prefix("--healthz-addr=") {
            return Some(addr.to_owned());
        }
    }
    None
}

/// Shared readiness state for a healthz listener.
///
/// New handles start not ready. Clones update and observe the same state.
#[derive(Clone, Default)]
pub struct Readiness(Arc<AtomicBool>);

impl Readiness {
    /// Mark the process ready to serve traffic.
    pub fn ready(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Mark the process not ready to serve traffic.
    pub fn not_ready(&self) {
        self.0.store(false, Ordering::SeqCst);
    }

    /// Return whether the process is ready to serve traffic.
    pub fn is_ready(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Start the optional healthz listener and return its readiness handle.
///
/// Returns `Ok(None)` without opening a listener when `healthz_addr` is `None`.
/// Otherwise, this binds the listener before returning, so address and bind
/// failures are reported to the caller. The spawned server starts not ready;
/// call [`Readiness::ready`] at the process-specific readiness point.
pub async fn init(flags: &HealthzFlags) -> anyhow::Result<Option<Readiness>> {
    let Some(raw_addr) = flags.healthz_addr.as_deref() else {
        return Ok(None);
    };
    let addr = parse_addr(raw_addr)?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind healthz listener at `{addr}`"))?;
    let readiness = Readiness::default();
    let app = healthz_router(readiness.clone());

    tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app).await {
            tracing::error!(%error, "Healthz listener failed");
        }
    });
    tracing::info!(%addr, "Healthz listener started");

    Ok(Some(readiness))
}

fn parse_addr(raw_addr: &str) -> anyhow::Result<SocketAddr> {
    let normalized = raw_addr
        .strip_prefix(':')
        .map(|port| format!("0.0.0.0:{port}"))
        .unwrap_or_else(|| raw_addr.to_owned());

    normalized
        .parse()
        .with_context(|| format!("invalid healthz address `{raw_addr}`; expected IP:PORT or :PORT"))
}

fn healthz_router(readiness: Readiness) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .with_state(readiness)
}

async fn healthz(State(readiness): State<Readiness>) -> StatusCode {
    if readiness.is_ready() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt as _;

    #[test]
    fn blank_host_binds_all_interfaces() {
        assert_eq!(
            parse_addr(":8080").expect("blank-host address should parse"),
            "0.0.0.0:8080".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn host_and_port_parse_as_socket_address() {
        assert_eq!(
            parse_addr("127.0.0.1:9000").expect("socket address should parse"),
            "127.0.0.1:9000".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn invalid_address_is_rejected() {
        let error = parse_addr("not-an-address").expect_err("invalid address should fail");
        assert!(error.to_string().contains("invalid healthz address"));
    }

    #[test]
    fn healthz_addr_parses_separate_equals_and_absent_forms() {
        assert_eq!(
            healthz_addr_from_args([
                "harnx-tool".to_owned(),
                "--healthz-addr".to_owned(),
                ":8080".to_owned(),
            ]),
            Some(":8080".to_owned())
        );
        assert_eq!(
            healthz_addr_from_args([
                "harnx-tool".to_owned(),
                "--healthz-addr=127.0.0.1:9000".to_owned(),
            ]),
            Some("127.0.0.1:9000".to_owned())
        );
        assert_eq!(
            healthz_addr_from_args(["harnx-tool".to_owned(), "--other-flag".to_owned()]),
            None
        );
    }

    #[tokio::test]
    async fn healthz_tracks_readiness_transitions() {
        let readiness = Readiness::default();

        assert_eq!(
            healthz_status(&readiness).await,
            StatusCode::SERVICE_UNAVAILABLE
        );
        readiness.ready();
        assert_eq!(healthz_status(&readiness).await, StatusCode::OK);
        readiness.not_ready();
        assert_eq!(
            healthz_status(&readiness).await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn disabled_healthz_is_a_noop() {
        let readiness = init(&HealthzFlags { healthz_addr: None })
            .await
            .expect("disabled healthz should be a no-op");

        assert!(readiness.is_none());
    }

    async fn healthz_status(readiness: &Readiness) -> StatusCode {
        healthz_router(readiness.clone())
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("request should succeed")
            .status()
    }
}
