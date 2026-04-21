//! LLM call wrappers that operate on pre-built `ChatCompletionsData`.
//! Harnx wrappers in `crates/harnx/src/client/common.rs` build
//! `ChatCompletionsData` from `Input + GlobalConfig` and delegate here.
//! Dry-run handling also stays on the harnx side because it requires
//! `Input + Config` to compute the echo text.

use anyhow::{Context, Result};
use harnx_client::{Client, ClientCallContext, SseHandler};
use harnx_core::abort::wait_abort_signal;
use harnx_core::api_types::{ChatCompletionsData, ChatCompletionsOutput};

/// Non-streaming LLM call. Builds the `reqwest::Client`, invokes
/// `Client::chat_completions_inner`, and wraps the error with a useful
/// context string. Mirrors the original harnx `chat_completions_with_input`
/// body minus the dry-run branch and the `ChatCompletionsData`
/// construction — both of which stay on the harnx caller.
pub async fn chat_completions_with_data(
    client: &dyn Client,
    data: ChatCompletionsData,
    ctx: &ClientCallContext<'_>,
) -> Result<ChatCompletionsOutput> {
    let reqwest_client = client.build_client(ctx)?;
    client
        .chat_completions_inner(&reqwest_client, data)
        .await
        .with_context(|| {
            format!(
                "Failed to call chat-completions api (client: {}, model: {})",
                client.name(),
                client.model().id()
            )
        })
}

/// Streaming LLM call. Races the underlying streaming inner method
/// against `wait_abort_signal`. On abort, notifies the handler via
/// `handler.done()` and returns Ok — the abort-signal mechanism is
/// how the caller communicates cancellation, so the streaming-side
/// doesn't need to surface an error.
pub async fn chat_completions_streaming_with_data(
    client: &dyn Client,
    data: ChatCompletionsData,
    handler: &mut SseHandler,
    ctx: &ClientCallContext<'_>,
) -> Result<()> {
    let abort_signal = handler.abort();
    tokio::select! {
        ret = async {
            let reqwest_client = client.build_client(ctx)?;
            client
                .chat_completions_streaming_inner(&reqwest_client, handler, data)
                .await
        } => {
            handler.done();
            ret.with_context(|| {
                format!(
                    "Failed to call chat-completions api (client: {}, model: {})",
                    client.name(),
                    client.model().id()
                )
            })
        }
        _ = wait_abort_signal(&abort_signal) => {
            handler.done();
            Ok(())
        }
    }
}
