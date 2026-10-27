//! OpenAI-compatible streamed responses with explicit test-controlled pacing.
use super::{Llm, Script};
use axum::{
    extract::State,
    response::{
        sse::{Event, Sse},
        IntoResponse,
    },
    Json,
};
use futures::stream;
use serde_json::{json, Value};
use std::{convert::Infallible, sync::Arc};

#[path = "completion/frames.rs"]
mod frames;
use frames::FramePlan;

pub(super) async fn completion(
    State(llm): State<Arc<Llm>>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    let first = {
        let mut requests = llm.requests.lock();
        let first = requests.is_empty();
        requests.push(body.clone());
        first
    };
    llm.requested.notify_one();
    if llm.script == Script::Fail {
        return (axum::http::StatusCode::BAD_REQUEST, Json(json!({"error":{"message":"secret-token /srv/private https://private.invalid", "type":"invalid_request_error"}}))).into_response();
    }
    assert_eq!(body["stream"], true, "worker must call streaming LLM");
    let plan = FramePlan::new(&llm, &body, first);
    Sse::new(stream::unfold(
        (0, plan, llm),
        move |(index, plan, llm)| async move {
            if index >= plan.frames.len() {
                return None;
            }
            if plan.waits_before(index) {
                llm.release.notified().await;
            }
            let event = Event::default().data(plan.frames[index].clone());
            Some((Ok::<_, Infallible>(event), (index + 1, plan, llm)))
        },
    ))
    .into_response()
}
