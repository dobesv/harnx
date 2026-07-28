use std::{path::PathBuf, sync::Arc};

use ag_ui_core::event::Event;
use harnx_core::{abort::AbortSignal, sink::with_agent_event_sink};
use harnx_runtime::{config::GlobalConfig, AgentCallFn};
use tokio::sync::{broadcast, mpsc};

use super::{build_input, build_loop_ctx, BroadcastEventSender};

pub(super) struct LocalTestTurnParams {
    pub prompt_config: GlobalConfig,
    pub call_fn: AgentCallFn,
    pub abort_signal: AbortSignal,
    pub inject_rx: mpsc::Receiver<String>,
    pub working_dir: Option<PathBuf>,
    pub event_tx: broadcast::Sender<Event>,
    pub text: String,
    pub attachment_refs: Vec<String>,
    pub sink: Arc<BroadcastEventSender>,
}

/// Legacy in-process executor used only when tests inject a model call function.
/// Production serve routing is thin-client-only because it never supplies one.
pub(super) async fn run_local_test_turn(
    params: LocalTestTurnParams,
) -> anyhow::Result<harnx_runtime::LoopResult> {
    let loop_ctx = build_loop_ctx(
        params.prompt_config.clone(),
        Some(params.call_fn),
        params.abort_signal,
        params.inject_rx,
        params.working_dir,
        params.event_tx,
    );
    let input = build_input(&params.prompt_config, &params.text, &params.attachment_refs)
        .expect("build actor input");
    with_agent_event_sink(params.sink, async {
        harnx_runtime::run_agent_loop(&loop_ctx, input).await
    })
    .await
}
