mod markdown;
mod stream;

pub use self::markdown::{MarkdownRender, RenderOptions};
use self::stream::{markdown_stream, raw_stream};

use crate::repl::input_queue::InputQueue;
use crate::utils::{error_text, pretty_error, AbortSignal, IS_STDOUT_TERMINAL};
use crate::{client::SseEvent, config::GlobalConfig};

use anyhow::Result;
use tokio::sync::mpsc::UnboundedReceiver;

pub async fn render_stream(
    rx: UnboundedReceiver<SseEvent>,
    config: &GlobalConfig,
    abort_signal: AbortSignal,
    spinner_message: &str,
    input_queue: Option<InputQueue>,
) -> Result<()> {
    let ret = if *IS_STDOUT_TERMINAL && config.read().highlight {
        let render_options = config.read().render_options()?;
        let mut render = MarkdownRender::init(render_options)?;
        markdown_stream(
            rx,
            &mut render,
            &abort_signal,
            spinner_message,
            input_queue.as_ref(),
        )
        .await
    } else {
        raw_stream(rx, &abort_signal, spinner_message, input_queue.as_ref()).await
    };
    ret.map_err(|err| err.context("Failed to reader stream"))
}

pub fn render_error(err: anyhow::Error) {
    eprintln!("{}", error_text(&pretty_error(&err)));
}
