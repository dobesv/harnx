use super::fixture::*;
use parking_lot::Mutex;
use serde_json::json;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tracing::{
    field::{Field, Visit},
    instrument::WithSubscriber,
    Subscriber,
};
use tracing_subscriber::{
    layer::{Context, SubscriberExt},
    Layer,
};

#[derive(Clone, Default)]
struct Capture(
    Arc<Mutex<Vec<HashMap<String, String>>>>,
    Arc<tokio::sync::Notify>,
);
#[derive(Default)]
struct Fields(HashMap<String, String>);
impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().to_string(), format!("{value:?}"));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
}
impl<S: Subscriber> Layer<S> for Capture {
    fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        if fields
            .0
            .get("message")
            .is_some_and(|text| text.contains("middleware started"))
        {
            self.1.notify_one();
        }
        self.0.lock().push(fields.0);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_traces_start_finish_and_failure_with_name() {
    let fixture = Fixture::new(vec![bash("exit 2")]).await;
    let capture = Capture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    fixture
        .eval(json!({}))
        .with_subscriber(subscriber)
        .await
        .unwrap();
    let events = capture.0.lock();
    for message in [
        "middleware started",
        "middleware failed",
        "middleware finished",
    ] {
        assert!(
            events.iter().any(|fields| fields
                .get("message")
                .is_some_and(|text| text.contains(message))
                && fields.get("middleware").is_some_and(|name| name == "test")),
            "{message}: {events:?}"
        );
    }
}

fn lease_lost() -> anyhow::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "middleware test lease lost",
    )
    .into()
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_idle_script_cancels_without_polling_admission() {
    let mut entry = bash("sleep 30; next");
    entry["on_error"] = json!("allow");
    let mut fixture = Fixture::new(vec![entry]).await;
    let boundaries = Arc::new(AtomicUsize::new(0));
    let observed = boundaries.clone();
    fixture.ctx.work_boundary = Some(Arc::new(move |_, _| {
        observed.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }));
    let capture = Capture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let abort = harnx_core::abort::create_abort_signal();
    let run = harnx_engine::tool::eval_tool_calls(&fixture.ctx, vec![call(json!({}))], &abort)
        .with_subscriber(subscriber);
    tokio::pin!(run);
    tokio::select! {
        result = &mut run => panic!("script finished before cancellation: {result:?}"),
        _ = capture.1.notified() => {},
        _ = tokio::time::sleep(Duration::from_secs(5)) => panic!("frame never started"),
    }
    let before = boundaries.load(Ordering::SeqCst);
    // Admission callbacks can commit effects; they aren't periodic lease probes.
    tokio::select! {
        result = &mut run => panic!("script finished before cancellation: {result:?}"),
        _ = tokio::time::sleep(Duration::from_millis(250)) => {},
    }
    assert_eq!(boundaries.load(Ordering::SeqCst), before);
    abort.set_ctrlc();
    assert!(tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .unwrap()
        .is_err());
    assert!(fixture.calls().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_allowlisted_call_uses_late_installed_admission_guard() {
    let mut entry = python("call_tool('probe_aux', {})");
    entry["tools"] = json!(["probe_aux"]);
    let mut fixture = Fixture::new(vec![entry]).await;
    fixture.ctx.work_boundary = Some(Arc::new(move |_, call| {
        Box::pin(async move {
            if call.name == "probe_aux" {
                Err(lease_lost())
            } else {
                Ok(())
            }
        })
    }));
    let error = fixture.eval(json!({})).await.err().unwrap();
    assert!(
        error.downcast_ref::<std::io::Error>().is_some(),
        "{error:#}"
    );
    assert!(fixture.calls().is_empty());
}
