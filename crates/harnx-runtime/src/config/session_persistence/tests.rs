use super::*;
use async_nats::jetstream::context::{PublishError, PublishErrorKind};
use harnx_toolset_server::reply_limits::is_size_error;

struct FailingSink {
    fatal: bool,
}

impl SessionAppendSink for FailingSink {
    fn append(&self, _entry: &SessionLogEntry) -> Result<u64> {
        Err(PublishError::new(PublishErrorKind::MaxPayloadExceeded).into())
    }

    fn failure_is_fatal(&self) -> bool {
        self.fatal
    }
}

fn session(fatal: bool) -> Session {
    let mut session =
        super::super::session::new(&super::super::Config::default(), "append-error-test", None)
            .unwrap();
    session.runtime = Some(Arc::new(
        Arc::new(FailingSink { fatal }) as Arc<dyn SessionAppendSink>
    ));
    session
}

#[test]
fn authoritative_batch_retains_first_append_cause_through_later_appends() {
    let mut session = session(true);
    let mut status = append_event_status(&mut session, &SessionLogEntry::Clear);
    status &= AppendStatus::from_result(Ok(Some(1)));
    status &= AppendStatus::from_result(Err(anyhow::anyhow!("later unrelated failure")));
    assert!(!status.is_complete());
    let error = require_authoritative_appends(&session, status, "tool results").unwrap_err();
    assert_eq!(error.to_string(), "failed to durably persist tool results");
    assert!(is_size_error(&error), "typed cause lost: {error:#}");
}

#[test]
fn non_authoritative_append_failure_remains_best_effort() {
    let mut session = session(false);
    let status = append_event_status(&mut session, &SessionLogEntry::Clear);
    assert!(!status.is_complete());
    require_authoritative_appends(&session, status, "tool results").unwrap();
    assert!(!append_event(&mut session, &SessionLogEntry::Clear));
}

#[test]
fn missing_sink_still_fails_a_batch_without_erasing_a_later_cause() {
    let mut session = session(true);
    let mut status =
        AppendStatus::from_result(append_through(None, session.id(), &SessionLogEntry::Clear));
    status &= append_event_status(&mut session, &SessionLogEntry::Clear);
    let error = require_authoritative_appends(&session, status, "user input").unwrap_err();
    assert!(is_size_error(&error), "typed cause lost: {error:#}");
}
