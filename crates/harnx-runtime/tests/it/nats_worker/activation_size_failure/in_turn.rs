//! Model stub and diagnostic assertions for in-turn size rejection.
use super::*;

pub(super) async fn set_stream_limit(
    js: &async_nats::jetstream::Context,
    key: &str,
    max_message_size: i32,
) -> Result<()> {
    let name = harnx_runtime::nats_session_log::stream_name_for_session(key);
    let mut config = js.get_stream(name).await?.get_info().await?.config;
    config.max_message_size = max_message_size;
    js.update_stream(config).await?;
    Ok(())
}

pub(super) fn call_fn(
    js: &async_nats::jetstream::Context,
    key: &str,
    calls: Arc<AtomicUsize>,
    oversized_diagnostic: bool,
) -> harnx_runtime::agent_loop::AgentCallFn {
    let js = js.clone();
    let key = key.to_owned();
    Arc::new(move |_input, _config, _abort| {
        let attempt = calls.fetch_add(1, Ordering::SeqCst);
        let js = js.clone();
        let key = key.clone();
        Box::pin(async move {
            if attempt == 0 && oversized_diagnostic {
                // Input and preparation are already durable. Only the real
                // assistant append and its diagnostic face this ceiling.
                set_stream_limit(&js, &key, 128).await?;
            }
            // Return a successful model result. The worker's normal
            // add_assistant_text -> append_through path must fail it.
            let response = if attempt == 0 {
                "x".repeat(32768)
            } else {
                "done".into()
            };
            Ok((response, None, vec![], CompletionTokenUsage::default()))
        })
    })
}

pub(super) fn assert_diagnostic(entries: &[(u64, SessionLogEntry)], oversized: bool) {
    let errors = error_messages(entries);
    if oversized {
        assert!(errors.is_empty(), "the diagnostic itself cannot fit");
        return;
    }
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("failed to durably persist assistant response"));
    assert!(errors[0].contains("JetStream did not ack session log entry"));
}
