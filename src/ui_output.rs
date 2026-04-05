use std::sync::OnceLock;
use tokio::sync::mpsc::UnboundedSender;

static UI_OUTPUT_SENDER: OnceLock<UnboundedSender<String>> = OnceLock::new();

pub fn install_ui_output_sender(sender: UnboundedSender<String>) {
    let _ = UI_OUTPUT_SENDER.set(sender);
}

pub fn emit_ui_output(text: impl Into<String>) -> bool {
    match UI_OUTPUT_SENDER.get() {
        Some(sender) => sender.send(text.into()).is_ok(),
        None => false,
    }
}
