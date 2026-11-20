//! Completion records contain protocol metadata only, never message parts or credentials.

use a2a_lf::{A2AError, ListTasksResponse, Message, SendMessageResponse, Task, TaskState};
use std::time::Instant;

pub(crate) mod http;
mod stream;

#[cfg(test)]
mod tests;

pub(crate) const TARGET: &str = "harnx_a2a_server::requests";
const MAX_LOG_ID_CHARS: usize = 256;

fn bounded_id(value: &str) -> String {
    // IDs can come from unauthenticated rejected requests, not just our store.
    let mut id: String = value.chars().take(MAX_LOG_ID_CHARS).collect();
    if id.len() < value.len() {
        id.push('…');
    }
    id
}

pub(crate) struct RequestLog {
    export: String,
    method: &'static str,
    started: Instant,
    task_id: Option<String>,
    context_id: Option<String>,
    message_id: Option<String>,
    state: Option<TaskState>,
    tasks_returned: Option<usize>,
    error: Option<(i32, String)>,
    events: Option<u64>,
    end_reason: Option<&'static str>,
    http_status: Option<u16>,
    finished: bool,
}

impl RequestLog {
    pub(crate) fn new(export: &str, method: &'static str) -> Self {
        Self {
            export: export.to_owned(),
            method,
            started: Instant::now(),
            task_id: None,
            context_id: None,
            message_id: None,
            state: None,
            tasks_returned: None,
            error: None,
            events: None,
            end_reason: None,
            http_status: None,
            finished: false,
        }
    }

    pub(crate) fn with_task(mut self, task_id: &str) -> Self {
        self.task_id = Some(bounded_id(task_id));
        self
    }

    pub(crate) fn with_context(mut self, context_id: Option<&str>) -> Self {
        self.context_id = context_id.map(bounded_id);
        self
    }

    pub(crate) fn with_message(mut self, message: &Message) -> Self {
        self.task_id = message.task_id.as_deref().map(bounded_id);
        self.context_id = message.context_id.as_deref().map(bounded_id);
        self.message_id = Some(bounded_id(&message.message_id));
        self
    }

    pub(crate) fn task(&mut self, task: &Task) {
        self.task_id = Some(bounded_id(&task.id));
        self.context_id = Some(bounded_id(&task.context_id));
        self.state = Some(task.status.state.clone());
    }

    pub(crate) fn send_response(&mut self, response: &SendMessageResponse) {
        match response {
            SendMessageResponse::Task(task) => self.task(task),
            SendMessageResponse::Message(message) => {
                self.task_id = message.task_id.as_deref().map(bounded_id);
                self.context_id = message.context_id.as_deref().map(bounded_id);
            }
        }
    }

    pub(crate) fn list_response(&mut self, response: &ListTasksResponse) {
        self.tasks_returned = Some(response.tasks.len());
    }

    pub(crate) fn complete<T>(
        mut self,
        result: Result<T, A2AError>,
        observe: impl FnOnce(&mut Self, &T),
    ) -> Result<T, A2AError> {
        match &result {
            Ok(response) => observe(&mut self, response),
            Err(error) => self.error(error),
        }
        self.finish();
        result
    }

    pub(crate) fn error(&mut self, error: &A2AError) {
        // These errors echo raw client values from parts/headers or IDs. Keep the
        // category in logs, while leaving the detailed wire response unchanged.
        let message = match error.code {
            a2a_lf::error_code::CONTENT_TYPE_NOT_SUPPORTED => "unsupported raw mediaType",
            a2a_lf::error_code::VERSION_NOT_SUPPORTED => "A2A version not supported",
            a2a_lf::error_code::TASK_NOT_CANCELABLE => "task cannot be canceled",
            _ => &error.message,
        };
        self.error = Some((error.code, message.to_owned()));
    }

    fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        tracing::info!(
            target: TARGET,
            export = %self.export,
            method = self.method,
            task_id = self.task_id.as_deref(),
            context_id = self.context_id.as_deref(),
            message_id = self.message_id.as_deref(),
            state = self.state.as_ref().map(tracing::field::debug),
            tasks_returned = self.tasks_returned,
            error_code = self.error.as_ref().map(|error| error.0),
            error_message = self.error.as_ref().map(|error| error.1.as_str()),
            duration_ms = self.started.elapsed().as_secs_f64() * 1000.0,
            events_sent = self.events,
            end_reason = self.end_reason,
            http_status = self.http_status,
            "A2A request finished"
        );
    }
}

impl Drop for RequestLog {
    fn drop(&mut self) {
        self.finish();
    }
}
