//! Hooks for discovery and rejections before typed protocol dispatch.

use super::RequestLog;
use a2a_lf::A2AError;
use axum::{
    extract::{Request, State},
    http::Method,
    middleware::Next,
    response::Response,
};
use serde_json::Value;
use std::time::Instant;

#[derive(Clone)]
struct Started {
    export: String,
    at: Instant,
    card: bool,
}

#[derive(Clone)]
struct LoggedRejection;

/// Must stay outermost on the per-export router: identity/access/compatibility
/// rejection hooks need Started before they run, and card logs need their response marker.
pub(crate) async fn start(
    State(export): State<String>,
    mut request: Request,
    next: Next,
) -> Response {
    let card = matches!(*request.method(), Method::GET | Method::HEAD)
        && matches!(
            request.uri().path(),
            "/.well-known/agent-card.json" | "/.well-known/agent.json"
        );
    let started = Started {
        export,
        at: Instant::now(),
        card,
    };
    request.extensions_mut().insert(started.clone());
    let response = next.run(request).await;
    if card && response.extensions().get::<LoggedRejection>().is_none() {
        let mut log = started.log("AgentCard");
        log.http_status = Some(response.status().as_u16());
    }
    response
}

impl Started {
    fn log(&self, method: &'static str) -> RequestLog {
        let mut log = RequestLog::new(&self.export, method);
        log.started = self.at;
        log
    }
}

/// The body has already been read by identity or compatibility middleware.
/// Only copy known ID fields, never arbitrary method names or parameter values.
pub(crate) fn rejected(
    request: &Request,
    body: Option<&Value>,
    response: &mut Response,
    error: Option<&A2AError>,
) {
    let Some(started) = request.extensions().get::<Started>() else {
        return;
    };
    let method = if started.card {
        "AgentCard"
    } else {
        method(body)
    };
    let mut log = started.log(method);
    if let Some(body) = body {
        log.request_ids(body);
    }
    log.http_status = Some(response.status().as_u16());
    if let Some(error) = error {
        log.error(error);
    }
    if matches!(method, "SendStreamingMessage" | "SubscribeToTask") {
        log.events = Some(0);
        log.end_reason = Some("error");
    }
    response.extensions_mut().insert(LoggedRejection);
}

impl RequestLog {
    fn request_ids(&mut self, body: &Value) {
        let params = &body["params"];
        let (task, context, message) = match self.method {
            "SendMessage" | "SendStreamingMessage" => (
                params["message"]["taskId"].as_str(),
                params["message"]["contextId"].as_str(),
                params["message"]["messageId"].as_str(),
            ),
            "ListTasks" => (None, params["contextId"].as_str(), None),
            "GetTask" | "CancelTask" | "SubscribeToTask" => (params["id"].as_str(), None, None),
            "CreateTaskPushNotificationConfig"
            | "GetTaskPushNotificationConfig"
            | "ListTaskPushNotificationConfigs"
            | "DeleteTaskPushNotificationConfig" => (params["taskId"].as_str(), None, None),
            _ => (None, None, None),
        };
        self.task_id = task.map(super::bounded_id);
        self.context_id = context.map(super::bounded_id);
        self.message_id = message.map(super::bounded_id);
    }
}

fn method(body: Option<&Value>) -> &'static str {
    let raw = body.and_then(|body| body["method"].as_str()).unwrap_or("");
    match crate::compat::canonical_method(raw) {
        "SendMessage" => "SendMessage",
        "SendStreamingMessage" => "SendStreamingMessage",
        "GetTask" => "GetTask",
        "CancelTask" => "CancelTask",
        "SubscribeToTask" => "SubscribeToTask",
        "ListTasks" => "ListTasks",
        "GetExtendedAgentCard" => "GetExtendedAgentCard",
        "CreateTaskPushNotificationConfig" => "CreateTaskPushNotificationConfig",
        "GetTaskPushNotificationConfig" => "GetTaskPushNotificationConfig",
        "ListTaskPushNotificationConfigs" => "ListTaskPushNotificationConfigs",
        "DeleteTaskPushNotificationConfig" => "DeleteTaskPushNotificationConfig",
        _ => "Unknown",
    }
}

pub(crate) fn caller(request: &Request, identity: &crate::identity::RequestIdentity) {
    if let Some(started) = request.extensions().get::<Started>() {
        tracing::debug!(
            target: super::TARGET,
            export = %started.export,
            user_id = identity.principal.user_id(),
            "A2A caller resolved"
        );
    }
}

pub(crate) fn compatibility_rejection(
    request: &Request,
    body: Option<&Value>,
    mut response: Response,
) -> Response {
    let error = response.extensions().get::<A2AError>().cloned();
    rejected(request, body, &mut response, error.as_ref());
    response
}
