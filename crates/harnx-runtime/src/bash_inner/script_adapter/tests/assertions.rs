use super::{InnerCallRun, InnerTraceStatus, Ordering, Provider, ScriptInnerExecutor};

pub(super) fn assert_finished_concurrency(provider: &Provider, maximum: usize) {
    assert_eq!(provider.maximum.load(Ordering::SeqCst), maximum);
    assert_eq!(provider.active.load(Ordering::SeqCst), 0);
}

pub(super) fn assert_round_calls(provider: &Provider, count: usize) {
    let calls = provider.calls.lock().unwrap();
    assert_eq!(calls.len(), count);
    assert!(calls.iter().all(|call| call.2 == Some(42)));
}

pub(super) fn assert_trace(run: &InnerCallRun, count: usize, status: InnerTraceStatus) {
    let trace = run.trace();
    assert_eq!(
        trace.len(),
        count,
        "every attempted call must leave a trace"
    );
    assert!(trace.iter().all(|entry| entry.status == status));
}

pub(super) fn assert_adapter_cancelled(
    run: &InnerCallRun,
    adapter: &ScriptInnerExecutor,
    provider: &Provider,
) {
    assert_eq!(provider.active.load(Ordering::SeqCst), 0);
    assert!(!adapter.abort.aborted());
    assert_trace(run, 2, InnerTraceStatus::Cancelled);
}
