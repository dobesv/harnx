//! Client-credentials proxy mode (T3): opt-in wiring, security boundaries,
//! integration flows, TLS identity and safe-error behavior.

pub mod fixture;

mod boundaries;
mod config;
mod flow;
mod safety;
#[cfg(unix)]
mod telemetry;
mod tls;
