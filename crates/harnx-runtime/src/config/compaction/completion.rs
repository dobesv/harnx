//! Execute a prepared compaction request through the usual completion engine.

use crate::client::{
    ChatCompletionsData, Client, ClientCallContext, CompletionTokenUsage, SseHandler,
};
use crate::config::{GlobalConfig, Input};
use crate::utils::AbortSignal;
use anyhow::Result;
use harnx_core::tool::ToolCall;

type CompletionResult = (String, Option<String>, Vec<ToolCall>, CompletionTokenUsage);

struct CompletionOptions {
    dry_run: bool,
    user_agent: Option<String>,
    guard_output: bool,
}

impl CompletionOptions {
    fn new(input: &Input, config: &GlobalConfig) -> Self {
        let cfg = config.read();
        Self {
            dry_run: cfg.dry_run,
            user_agent: cfg.user_agent.clone(),
            guard_output: cfg
                .loop_detection
                .resolve(input.agent().loop_detection())
                .output,
        }
    }

    fn client_context(&self) -> ClientCallContext<'_> {
        ClientCallContext {
            dry_run: self.dry_run,
            user_agent: self.user_agent.as_deref(),
        }
    }

    fn check_reply(&self, result: &CompletionResult) -> Result<()> {
        if self.guard_output {
            use harnx_core::loop_guard::{check_output, OutputChannel};
            check_output(
                OutputChannel::Thinking,
                result.1.as_deref().unwrap_or_default(),
            )?;
            check_output(OutputChannel::Answer, &result.0)?;
        }
        Ok(())
    }
}

pub(super) struct CompletionCall<'a> {
    client: &'a dyn Client,
    config: &'a GlobalConfig,
    options: CompletionOptions,
}

impl<'a> CompletionCall<'a> {
    pub(super) fn new(input: &Input, client: &'a dyn Client, config: &'a GlobalConfig) -> Self {
        Self {
            client,
            config,
            options: CompletionOptions::new(input, config),
        }
    }

    pub(super) async fn send(
        self,
        data: ChatCompletionsData,
        abort: AbortSignal,
    ) -> Result<CompletionResult> {
        if self.options.dry_run {
            return Ok(echo_request(data));
        }
        if data.stream {
            self.send_streaming(data, abort).await
        } else {
            self.send_nonstreaming(data, abort).await
        }
    }

    async fn send_streaming(
        &self,
        data: ChatCompletionsData,
        abort: AbortSignal,
    ) -> Result<CompletionResult> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let handler =
            SseHandler::new(tx, abort.clone()).with_output_guard(self.options.guard_output);
        let drainer = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let result = harnx_engine::chat_completions::run_chat_completion_streaming(
            self.client,
            data,
            &self.options.client_context(),
            handler,
            abort,
        )
        .await;
        let _ = drainer.await;
        let (text, thought, calls, usage, _) = result?;
        Ok((text, thought, calls, usage))
    }

    async fn send_nonstreaming(
        &self,
        data: ChatCompletionsData,
        abort: AbortSignal,
    ) -> Result<CompletionResult> {
        let result = crate::utils::abortable_run_with_spinner(
            harnx_engine::chat_completions::run_chat_completion(
                self.client,
                data,
                &self.options.client_context(),
                false,
                abort.clone(),
            ),
            "Compacting",
            abort,
        )
        .await?;
        self.options.check_reply(&result)?;
        if !result.0.is_empty() {
            self.config.read().print_markdown(&result.0)?;
        }
        Ok(result)
    }
}

fn echo_request(data: ChatCompletionsData) -> CompletionResult {
    let text = data
        .messages
        .iter()
        .map(|message| message.content.to_text())
        .collect::<Vec<_>>()
        .join("\n\n");
    crate::utils::emit_info(text.clone());
    (text, None, vec![], Default::default())
}
