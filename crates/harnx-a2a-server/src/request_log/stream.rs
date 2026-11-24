use super::RequestLog;
use a2a_lf::{A2AError, StreamResponse};
use futures::{stream::BoxStream, Stream, StreamExt};
use harnx_core::abort::{wait_abort_signal, AbortSignal};
use std::{
    pin::Pin,
    task::{Context, Poll},
};

type EventStream = BoxStream<'static, Result<StreamResponse, A2AError>>;

pub(crate) struct StreamLog {
    request: RequestLog,
    abort: AbortSignal,
}

impl RequestLog {
    pub(crate) fn streaming(mut self, abort: AbortSignal) -> StreamLog {
        self.events = Some(0);
        StreamLog {
            request: self,
            abort,
        }
    }

    fn event(&mut self, event: &Result<StreamResponse, A2AError>) {
        *self.events.get_or_insert(0) += 1;
        match event {
            Ok(StreamResponse::Task(task)) => self.task(task),
            Ok(StreamResponse::StatusUpdate(update)) => {
                self.task_id = Some(super::bounded_id(&update.task_id));
                self.context_id = Some(super::bounded_id(&update.context_id));
                self.state = Some(update.status.state.clone());
            }
            Ok(StreamResponse::ArtifactUpdate(update)) => {
                self.task_id = Some(super::bounded_id(&update.task_id));
                self.context_id = Some(super::bounded_id(&update.context_id));
            }
            Ok(StreamResponse::Message(_)) => (),
            Err(error) => {
                self.error(error);
                self.end_reason = Some("error");
            }
        }
        if event.is_ok() {
            self.record_boundary();
        }
    }

    fn record_boundary(&mut self) {
        if let Some(state) = &self.state {
            if state.is_terminal() {
                self.end_reason = Some("terminal");
            } else if crate::runner::ends_response(state) {
                self.end_reason = Some("interrupted");
            }
        }
    }
}

impl StreamLog {
    pub(crate) fn stream(
        mut self,
        result: Result<EventStream, A2AError>,
    ) -> Result<EventStream, A2AError> {
        match result {
            Ok(inner) => {
                let shutdown = self.abort.clone();
                let inner = inner
                    .take_until(async move { wait_abort_signal(&shutdown).await })
                    .boxed();
                Ok(LoggedStream { inner, log: self }.boxed())
            }
            Err(error) => {
                self.request.error(&error);
                self.request.end_reason = Some("error");
                Err(error)
            }
        }
    }

    fn finish(&mut self, fallback: &'static str) {
        let reason = if self.abort.aborted() {
            "server_shutdown"
        } else {
            fallback
        };
        self.request.end_reason.get_or_insert(reason);
        self.request.finish();
    }
}

impl Drop for StreamLog {
    fn drop(&mut self) {
        self.finish("client_disconnect");
    }
}

struct LoggedStream {
    inner: EventStream,
    log: StreamLog,
}

impl Stream for LoggedStream {
    type Item = Result<StreamResponse, A2AError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let result = self.inner.as_mut().poll_next(cx);
        match &result {
            Poll::Ready(Some(event)) => self.log.request.event(event),
            Poll::Ready(None) => self.log.finish("stream_closed"),
            Poll::Pending => (),
        }
        result
    }
}
