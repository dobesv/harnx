//! `SessionCtx` is the bundle of state that travels through the engine and
//! its callees on the hot path. Any function in a non-UI crate that needs
//! to emit user-visible output takes `&SessionCtx` and calls
//! `ctx.sink.emit(...)`. Terminal-only primitives (inbox for mid-turn input,
//! hook manager handles) are *not* on this struct yet; they'll be added in
//! later plans once the engine starts consuming `SessionCtx`.

use std::{path::PathBuf, sync::Arc};

use crate::{abort::AbortSignal, event::AgentEventSink};

pub struct SessionCtx {
    pub sink: Arc<dyn AgentEventSink>,
    pub abort: AbortSignal,
    pub session_id: String,
    pub cwd: PathBuf,
}

impl SessionCtx {
    pub fn new(
        sink: Arc<dyn AgentEventSink>,
        abort: AbortSignal,
        session_id: String,
        cwd: PathBuf,
    ) -> Self {
        Self {
            sink,
            abort,
            session_id,
            cwd,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{abort::create_abort_signal, event::NullSink};

    #[test]
    fn session_ctx_constructs() {
        let ctx = SessionCtx::new(
            Arc::new(NullSink),
            create_abort_signal(),
            "test-session".into(),
            PathBuf::from("/tmp"),
        );
        assert_eq!(ctx.session_id, "test-session");
        assert_eq!(ctx.cwd, PathBuf::from("/tmp"));
        assert!(!ctx.abort.aborted());
    }
}
