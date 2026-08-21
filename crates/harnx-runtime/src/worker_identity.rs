//! Readiness record advertised by worker daemons.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const READINESS_PROTOCOL: u8 = 2;

/// Diagnostic worker identity carried on the readiness subject.
///
/// Build identity is deliberately informational. Frontends admit workers by
/// protocol, session scope, worker id, and PID so separately compiled but
/// protocol-compatible frontend and worker binaries can run together.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct WorkerReadiness {
    protocol: u8,
    pub(crate) session_scope: String,
    pub(crate) worker_id: String,
    pub(crate) pid: u32,
    pub(crate) build: String,
}

impl WorkerReadiness {
    pub(crate) fn current(session_scope: impl Into<String>, worker_id: impl Into<String>) -> Self {
        Self {
            protocol: READINESS_PROTOCOL,
            session_scope: session_scope.into(),
            worker_id: worker_id.into(),
            pid: std::process::id(),
            build: current_build().to_string(),
        }
    }

    pub(crate) fn from_payload(payload: &[u8]) -> Result<Self> {
        let readiness: Self = serde_json::from_slice(payload)
            .context("local worker sent a legacy or invalid readiness marker")?;
        anyhow::ensure!(
            readiness.protocol == READINESS_PROTOCOL,
            "local worker uses unsupported readiness protocol {}",
            readiness.protocol
        );
        Ok(readiness)
    }

    pub(crate) fn payload(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(self).context("serialize local worker readiness")
    }

    pub(crate) fn validate_route(
        &self,
        expected_session_scope: &str,
        expected_worker_id: &str,
    ) -> Result<()> {
        anyhow::ensure!(
            self.session_scope == expected_session_scope,
            "local worker readiness scope mismatch: expected '{expected_session_scope}', got '{}'",
            self.session_scope
        );
        anyhow::ensure!(
            self.worker_id == expected_worker_id,
            "local worker readiness worker id mismatch: expected '{expected_worker_id}', got '{}'",
            self.worker_id
        );
        Ok(())
    }

    pub(crate) fn has_pid(&self, expected_pid: u32) -> bool {
        self.pid == expected_pid
    }
}

pub(crate) const fn current_build() -> &'static str {
    env!("HARNX_BUILD_SHA")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_rejects_invalid_payload() {
        let error = WorkerReadiness::from_payload(b"local")
            .expect_err("legacy readiness marker must be rejected");
        assert!(
            format!("{error:#}").contains("legacy or invalid readiness marker"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn readiness_rejects_unsupported_protocol() {
        let readiness = WorkerReadiness {
            protocol: READINESS_PROTOCOL + 1,
            session_scope: "__local__".to_string(),
            worker_id: "local-test".to_string(),
            pid: 42,
            build: "test-build".to_string(),
        };
        let payload = serde_json::to_vec(&readiness).expect("serialize test readiness");
        let error = WorkerReadiness::from_payload(&payload)
            .expect_err("unsupported readiness protocol must be rejected");
        assert!(
            error
                .to_string()
                .contains("unsupported readiness protocol 3"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn readiness_accepts_a_different_build_for_the_same_target() {
        let readiness = WorkerReadiness {
            protocol: READINESS_PROTOCOL,
            session_scope: "__local__".to_string(),
            worker_id: "local-test".to_string(),
            pid: 42,
            build: "independently-compiled".to_string(),
        };
        readiness
            .validate_route("__local__", "local-test")
            .expect("build is diagnostic only");
        assert!(readiness.has_pid(42));
    }

    #[test]
    fn readiness_rejects_route_mismatches_and_identifies_stale_pids() {
        let readiness = WorkerReadiness {
            protocol: READINESS_PROTOCOL,
            session_scope: "__local__".to_string(),
            worker_id: "local-test".to_string(),
            pid: 42,
            build: "test-build".to_string(),
        };
        assert!(readiness.validate_route("prod", "local-test").is_err());
        assert!(readiness
            .validate_route("__local__", "local-other")
            .is_err());
        assert!(!readiness.has_pid(43));
    }
}
