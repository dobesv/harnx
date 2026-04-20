//! harnx-local orchestration for the client layer. Provider trait +
//! impls live in `harnx-client`. This module adds the `call_chat_completions`
//! orchestrator (uses `&GlobalConfig` — an engine-level concern that
//! will migrate to `harnx-engine` in a later plan) + the `retry`
//! fallback wrapper + `message` rendering helpers, and preserves a
//! `crate::client::init_client` path that routes through the
//! test-client override (installed by harnx's test harness).

pub use harnx_client::*;

pub mod common;
pub mod message;
pub mod retry;

pub use common::{
    call_chat_completions, call_chat_completions_streaming, chat_completions_streaming_with_input,
    chat_completions_with_input, create_client_config, install_models_override,
};
pub use message::{patch_messages, render_message_input};

// Shadow harnx-client's `init_client` with a harnx-side wrapper that
// first consults the test-client override. All call sites inside harnx
// use `crate::client::init_client` and therefore pick up this wrapper.
pub fn init_client(clients: &[ClientConfig], model: &Model) -> anyhow::Result<Box<dyn Client>> {
    #[cfg(test)]
    if let Some(client) = take_test_client() {
        return Ok(Box::new(TestClient::new(client)));
    }

    harnx_client::init_client(clients, model)
}

#[cfg(test)]
static TEST_CLIENT: std::sync::OnceLock<std::sync::Mutex<Option<std::sync::Arc<dyn Client>>>> =
    std::sync::OnceLock::new();

/// Mutex that serializes tests using shared global state (test client,
/// UI output sender). Tests that call `set_test_client`, `emit_ui_output`,
/// or create a `Tui` should acquire this lock for their entire duration.
/// Prefer using [`TestStateGuard`] instead of acquiring this directly.
#[cfg(test)]
pub static TEST_CLIENT_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

/// RAII guard that holds [`TEST_CLIENT_LOCK`], installs a test client on
/// creation, and clears both the client and UI output sender on drop.
#[cfg(test)]
pub struct TestStateGuard<'a> {
    _lock: tokio::sync::MutexGuard<'a, ()>,
}

#[cfg(test)]
impl TestStateGuard<'_> {
    /// Acquire the global test-state lock and install `client`.
    pub async fn new(client: Option<std::sync::Arc<dyn Client>>) -> TestStateGuard<'static> {
        let lock = TEST_CLIENT_LOCK.lock().await;
        set_test_client_inner(client);
        TestStateGuard { _lock: lock }
    }

    /// Swap the test client while already holding the lock.
    pub fn set_client(&self, client: Option<std::sync::Arc<dyn Client>>) {
        set_test_client_inner(client);
    }
}

#[cfg(test)]
impl Drop for TestStateGuard<'_> {
    fn drop(&mut self) {
        set_test_client_inner(None);
        crate::ui_output::clear_ui_output_sender();
    }
}

#[cfg(test)]
fn set_test_client_inner(client: Option<std::sync::Arc<dyn Client>>) {
    let slot = TEST_CLIENT.get_or_init(|| std::sync::Mutex::new(None));
    *slot.lock().expect("test client mutex poisoned") = client;
}

#[cfg(test)]
pub(crate) fn take_test_client() -> Option<std::sync::Arc<dyn Client>> {
    TEST_CLIENT
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .expect("test client mutex poisoned")
        .clone()
}

#[cfg(test)]
pub(crate) struct TestClient {
    inner: std::sync::Arc<dyn Client>,
    model: Model,
}

#[cfg(test)]
impl TestClient {
    pub(crate) fn new(inner: std::sync::Arc<dyn Client>) -> Self {
        let model = inner.model().clone();
        Self { inner, model }
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl Client for TestClient {
    fn extra_config(&self) -> Option<&ExtraConfig> {
        self.inner.extra_config()
    }

    fn patch_config(&self) -> Option<&RequestPatch> {
        self.inner.patch_config()
    }

    fn name(&self) -> &str {
        self.inner.name()
    }

    fn model(&self) -> &Model {
        &self.model
    }

    fn model_mut(&mut self) -> &mut Model {
        &mut self.model
    }

    async fn chat_completions_inner(
        &self,
        client: &reqwest::Client,
        data: ChatCompletionsData,
    ) -> anyhow::Result<ChatCompletionsOutput> {
        self.inner.chat_completions_inner(client, data).await
    }

    async fn chat_completions_streaming_inner(
        &self,
        client: &reqwest::Client,
        handler: &mut SseHandler,
        data: ChatCompletionsData,
    ) -> anyhow::Result<()> {
        self.inner
            .chat_completions_streaming_inner(client, handler, data)
            .await
    }

    async fn embeddings_inner(
        &self,
        client: &reqwest::Client,
        data: &EmbeddingsData,
    ) -> anyhow::Result<EmbeddingsOutput> {
        self.inner.embeddings_inner(client, data).await
    }

    async fn rerank_inner(
        &self,
        client: &reqwest::Client,
        data: &RerankData,
    ) -> anyhow::Result<RerankOutput> {
        self.inner.rerank_inner(client, data).await
    }
}
