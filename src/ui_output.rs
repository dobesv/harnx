#[cfg(test)]
use std::sync::Mutex;
#[cfg(not(test))]
use std::sync::OnceLock;
use tokio::sync::mpsc::UnboundedSender;

#[cfg(test)]
static UI_OUTPUT_SENDER: Mutex<Option<UnboundedSender<String>>> = Mutex::new(None);

#[cfg(not(test))]
static UI_OUTPUT_SENDER: OnceLock<UnboundedSender<String>> = OnceLock::new();

#[cfg(not(test))]
pub fn install_ui_output_sender(sender: UnboundedSender<String>) {
    let _ = UI_OUTPUT_SENDER.set(sender);
}

#[cfg(test)]
pub fn install_ui_output_sender(sender: UnboundedSender<String>) {
    let mut guard = UI_OUTPUT_SENDER
        .lock()
        .expect("UI_OUTPUT_SENDER mutex poisoned");
    *guard = Some(sender);
}

#[cfg(not(test))]
pub fn emit_ui_output(text: impl Into<String>) -> bool {
    match UI_OUTPUT_SENDER.get() {
        Some(sender) => sender.send(text.into()).is_ok(),
        None => false,
    }
}

#[cfg(test)]
pub fn emit_ui_output(text: impl Into<String>) -> bool {
    let guard = UI_OUTPUT_SENDER
        .lock()
        .expect("UI_OUTPUT_SENDER mutex poisoned");
    match guard.as_ref() {
        Some(sender) => sender.send(text.into()).is_ok(),
        None => false,
    }
}

#[cfg(test)]
pub fn clear_ui_output_sender() {
    let mut guard = UI_OUTPUT_SENDER
        .lock()
        .expect("UI_OUTPUT_SENDER mutex poisoned");
    *guard = None;
}
