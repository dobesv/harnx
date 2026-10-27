use a2a_lf::Message;
use harnx_runtime::{ApprovalRef, NatsSession};

/// Bundles the reply message and target reference to keep function signatures <= 4 args.
pub(crate) struct ReplyTarget<'a> {
    pub session: &'a NatsSession,
    pub request: &'a ApprovalRef,
    pub message: &'a Message,
    pub fingerprint: &'a str,
}
