use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

const FIRST_NOTICE: Duration = Duration::from_secs(5);
const REPEAT_NOTICE: Duration = Duration::from_secs(10);

/// Test-only seam: `HARNX_TEST_BROKER_NOTICE_MS` shrinks both notice
/// intervals so frontend/e2e tests can observe routing without
/// multi-second sleeps. This changes a status cadence, never a deadline,
/// and the sandbox runner does not pass the variable through.
fn notice_intervals() -> (Duration, Duration) {
    match std::env::var("HARNX_TEST_BROKER_NOTICE_MS") {
        Ok(value) => match value.parse::<u64>() {
            Ok(millis) if millis > 0 => {
                (Duration::from_millis(millis), Duration::from_millis(millis))
            }
            _ => (FIRST_NOTICE, REPEAT_NOTICE),
        },
        Err(_) => (FIRST_NOTICE, REPEAT_NOTICE),
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) enum WaitPhase {
    OtherOwner,
    Version(u32),
    PortReport(u32),
    Readiness(u32),
}

/// Status for the frontend's broker-only notice route. Contains no credentials.
#[derive(Clone, Copy, Debug)]
pub struct BrokerWaitNotice {
    phase: WaitPhase,
    elapsed: Duration,
}

#[cfg(test)]
impl BrokerWaitNotice {
    pub(super) fn test_notice(pid: u32) -> Self {
        Self {
            phase: WaitPhase::PortReport(pid),
            elapsed: FIRST_NOTICE,
        }
    }
}

impl fmt::Display for WaitPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OtherOwner => write!(f, "another owner to make NATS ready"),
            Self::Version(pid) => write!(f, "nats-server (PID {pid}) to report its version"),
            Self::PortReport(pid) => write!(f, "nats-server (PID {pid}) to report its port"),
            Self::Readiness(pid) => write!(f, "nats-server (PID {pid}) to accept connections"),
        }
    }
}

// qual:allow(dry, boilerplate) reason: "One Display impl uses elapsed whole seconds; derive_more is not a workspace dependency."
impl fmt::Display for BrokerWaitNotice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Waiting for {} ({}s elapsed). Press Ctrl-C to stop waiting.",
            self.phase,
            self.elapsed.as_secs(),
        )
    }
}

/// Frontend callbacks should dispatch synchronously without blocking and must
/// not call back into the broker scope. The supervisor looks up the current
/// route on every notice rather than inheriting a request's event sink.
pub type BrokerNoticeSink = Arc<dyn Fn(BrokerWaitNotice) + Send + Sync>;

pub(super) struct WaitNotices {
    started: Instant,
    next: Instant,
    repeat: Duration,
    report: Option<BrokerNoticeSink>,
}

impl WaitNotices {
    pub(super) fn new(report: Option<BrokerNoticeSink>) -> Self {
        let (first, repeat) = notice_intervals();
        let started = Instant::now();
        Self {
            started,
            next: started + first,
            repeat,
            report,
        }
    }

    pub(super) fn waiting(&mut self, phase: WaitPhase) {
        let now = Instant::now();
        if now < self.next {
            return;
        }
        self.next = now + self.repeat;
        if let Some(report) = &self.report {
            report(BrokerWaitNotice {
                phase,
                elapsed: now.duration_since(self.started),
            });
        }
    }
}
