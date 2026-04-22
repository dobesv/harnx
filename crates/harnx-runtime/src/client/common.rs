//! Engine-level orchestration around the provider `Client` trait.
//!
//! - `call_chat_completions` / `call_chat_completions_streaming` tie a
//!   `Client` implementation to `GlobalConfig`, the UI spinner, tool
//!   call evaluation, and markdown rendering.
//! - `create_config` / `create_openai_compatible_client_config` drive
//!   the interactive `.setup-client` provider configuration flow using
//!   `inquire` prompts and a harnx-local spinner.
//!
//! Provider trait + HTTP helpers live in `harnx-client`.

use super::*;

use crate::{
    config::{Config, GlobalConfig, Input},
    tool::{eval_tool_calls, ToolResult},
    utils::*,
};

use anyhow::{bail, Result};
use inquire::{
    list_option::ListOption, required, validator::Validation, MultiSelect, Select, Text,
};
use serde_json::{json, Value};
use std::sync::LazyLock;
use tokio::sync::mpsc::unbounded_channel;

/// Input-aware wrapper around `Client::chat_completions_inner` that
/// handles the dry-run short-circuit, `reqwest::Client` construction,
/// and building `ChatCompletionsData` from `Input`. Lives in harnx
/// because `Input` depends on `GlobalConfig`; `harnx-client` exposes
/// only the `ChatCompletionsData`-based inner method.
pub async fn chat_completions_with_input(
    client: &dyn Client,
    input: Input,
    config: &GlobalConfig,
    ctx: &ClientCallContext<'_>,
) -> Result<ChatCompletionsOutput> {
    if ctx.dry_run {
        let content = crate::config::input::echo_messages(&input, config);
        return Ok(ChatCompletionsOutput::new(&content));
    }
    let data =
        crate::config::input::prepare_completion_data(&input, config, client.model(), false)?;
    harnx_engine::chat_completions::chat_completions_with_data(client, data, ctx).await
}

/// Input-aware streaming wrapper — same role as
/// `chat_completions_with_input` for the streaming path. Respects the
/// caller's abort signal attached to `handler`.
pub async fn chat_completions_streaming_with_input(
    client: &dyn Client,
    input: &Input,
    config: &GlobalConfig,
    handler: &mut SseHandler,
    ctx: &ClientCallContext<'_>,
) -> Result<()> {
    if ctx.dry_run {
        let content = crate::config::input::echo_messages(input, config);
        handler.text(&content)?;
        handler.done();
        return Ok(());
    }
    let data = crate::config::input::prepare_completion_data(input, config, client.model(), true)?;
    harnx_engine::chat_completions::chat_completions_streaming_with_data(client, data, handler, ctx)
        .await
}

/// Install harnx's models-override (loaded from the user's config dir)
/// into the `harnx-client::ALL_PROVIDER_MODELS` static. Must be called
/// before any client initialization triggers `ALL_PROVIDER_MODELS`
/// evaluation.
pub fn install_models_override() {
    LazyLock::force(&MODELS_OVERRIDE_INSTALLED);
}

static MODELS_OVERRIDE_INSTALLED: LazyLock<()> = LazyLock::new(|| {
    if let Ok(models) = Config::loal_models_override() {
        harnx_client::install_models_override(models);
    }
});

fn spinner_label(config: &GlobalConfig) -> String {
    let config = config.read();
    // Icons omitted — the spinner braille frame serves as the leading character
    let status = config.render_status_line(false);
    if let Some(session) = &config.session {
        let su = session.completion_usage();
        if !su.is_empty() {
            if status.is_empty() {
                return su.to_string();
            }
            return format!("{}    {}", status, su);
        }
    }
    if status.is_empty() {
        "Generating".to_string()
    } else {
        status
    }
}

pub async fn call_chat_completions(
    input: &Input,
    print: bool,
    extract_code: bool,
    client: &dyn Client,
    config: &GlobalConfig,
    abort_signal: AbortSignal,
) -> Result<(
    String,
    Option<String>,
    Vec<ToolResult>,
    CompletionTokenUsage,
)> {
    let spinner_message = spinner_label(config);
    // Snapshot the config values we need into owned storage so the ctx
    // reference can live across the .await without holding the RwLock.
    let (dry_run, user_agent) = {
        let cfg = config.read();
        (cfg.dry_run, cfg.user_agent.clone())
    };
    let ctx = ClientCallContext {
        user_agent: user_agent.as_deref(),
        dry_run,
    };

    // Dry-run stays on the harnx side: the echo-message path needs
    // Input + config to walk the session. The engine only handles the
    // live LLM call. Match pre-plan behavior: print via print_markdown
    // when `print=true`.
    if dry_run {
        let content = crate::config::input::echo_messages(input, config);
        let usage = CompletionTokenUsage::default();
        if print && !content.is_empty() {
            config.read().print_markdown(&content)?;
        }
        return Ok((content, None, vec![], usage));
    }

    let data = crate::config::input::prepare_completion_data(input, config, client.model(), false)?;

    let engine_ret = abortable_run_with_spinner(
        harnx_engine::chat_completions::run_chat_completion(
            client,
            data,
            &ctx,
            extract_code,
            print, // suppress_final_output = print (we'll display via print_markdown)
            abort_signal.clone(),
        ),
        &spinner_message,
        abort_signal.clone(),
    )
    .await;

    match engine_ret {
        Ok((text, thought, tool_calls, usage)) => {
            if print {
                if let Some(v) = &thought {
                    config
                        .read()
                        .print_markdown(&format!("<think>\n{}\n</think>\n\n", v))?;
                }
                if !text.is_empty() {
                    config.read().print_markdown(&text)?;
                }
            }
            let tool_results = eval_tool_calls(
                &crate::tool::build_tool_eval_context(config),
                tool_calls,
                &abort_signal,
            )?;
            Ok((text, thought, tool_results, usage))
        }
        Err(err) => Err(err),
    }
}

pub async fn call_chat_completions_streaming(
    input: &Input,
    client: &dyn Client,
    config: &GlobalConfig,
    abort_signal: AbortSignal,
) -> Result<(
    String,
    Option<String>,
    Vec<ToolResult>,
    CompletionTokenUsage,
)> {
    let (dry_run, user_agent) = {
        let cfg = config.read();
        (cfg.dry_run, cfg.user_agent.clone())
    };
    let ctx = ClientCallContext {
        user_agent: user_agent.as_deref(),
        dry_run,
    };

    // Dry-run: just echo the prepared messages to stdout. The streaming
    // rendering path is unnecessary for a no-LLM echo.
    if dry_run {
        let content = crate::config::input::echo_messages(input, config);
        if !content.is_empty() {
            println!("{content}");
        }
        return Ok((content, None, vec![], CompletionTokenUsage::default()));
    }

    let data = crate::config::input::prepare_completion_data(input, config, client.model(), true)?;
    let (tx, rx) = unbounded_channel();
    let handler = SseHandler::new(tx, abort_signal.clone());

    // Compute rich spinner label from config; emit as Status so
    // CliAgentEventSink picks it up as the spinner message.
    let spinner_message = spinner_label(config);
    {
        use harnx_core::event::{AgentEvent, StatusLine};
        harnx_core::sink::emit_agent_event(AgentEvent::Status(StatusLine {
            text: spinner_message,
        }));
    }

    // Emit Turn::Started so the installed sink starts a "Generating" spinner.
    {
        use harnx_core::event::{AgentEvent, TurnEvent};
        harnx_core::sink::emit_agent_event(AgentEvent::Turn(TurnEvent::Started));
    }

    // Drain the SseHandler's channel — chunk display is now driven by
    // AgentEvent::Model::MessageChunk/ThoughtChunk emitted by SseHandler,
    // which the installed sink renders. The channel itself just needs
    // a consumer so the handler doesn't block on send.
    let drainer = tokio::spawn(async move {
        let mut rx = rx;
        while rx.recv().await.is_some() {}
    });

    let engine_ret = harnx_engine::chat_completions::run_chat_completion_streaming(
        client,
        data,
        &ctx,
        handler,
        abort_signal.clone(),
    )
    .await;

    let _ = drainer.await;

    // Emit Turn::Ended so the sink stops the spinner, exits raw mode,
    // and emits a trailing newline if needed.
    {
        use harnx_core::event::{AgentEvent, TurnEvent, TurnOutcome};
        harnx_core::sink::emit_agent_event(AgentEvent::Turn(TurnEvent::Ended {
            outcome: TurnOutcome::default(),
        }));
    }

    let (text, thought, tool_calls, usage, _aborted) = engine_ret?;

    let tool_results = if tool_calls.is_empty() {
        vec![]
    } else {
        eval_tool_calls(
            &crate::tool::build_tool_eval_context(config),
            tool_calls,
            &abort_signal,
        )?
    };

    Ok((text, thought, tool_results, usage))
}

pub async fn create_config(
    prompts: &[PromptAction<'static>],
    client: &str,
) -> Result<(String, Value)> {
    let mut config = json!({
        "type": client,
    });
    for (key, desc, help_message) in prompts {
        let env_name = format!("{client}_{key}").to_ascii_uppercase();
        let required = std::env::var(&env_name).is_err();
        let value = prompt_input_string(desc, required, *help_message)?;
        if !value.is_empty() {
            config[key] = value.into();
        }
    }
    let model = set_client_models_config(&mut config, client).await?;
    let clients = json!(vec![config]);
    Ok((model, clients))
}

pub async fn create_openai_compatible_client_config(
    client: &str,
) -> Result<Option<(String, Value)>> {
    let api_base =
        harnx_client::openai_compatible_api_base(client).unwrap_or("http(s)://{API_ADDR}/v1");

    let name = if harnx_client::is_openai_compatible_provider_name(client) {
        let value = prompt_input_string("Provider Name", true, None)?;
        value.replace(' ', "-")
    } else {
        client.to_string()
    };

    let mut config = json!({
        "type": harnx_client::OpenAICompatibleClient::NAME,
        "name": &name,
    });

    let api_base = if api_base.contains('{') {
        prompt_input_string("API Base", true, Some(&format!("e.g. {api_base}")))?
    } else {
        api_base.to_string()
    };
    config["api_base"] = api_base.into();

    let api_key = prompt_input_string("API Key", false, None)?;
    if !api_key.is_empty() {
        config["api_key"] = api_key.into();
    }

    let model = set_client_models_config(&mut config, &name).await?;
    let clients = json!(vec![config]);
    Ok(Some((model, clients)))
}

/// Hand-written dispatcher that replaces the macro-generated
/// `create_client_config` — kept in harnx so that its inquire/spinner
/// dependencies don't leak into `harnx-client`.
pub async fn create_client_config(client: &str) -> Result<(String, Value)> {
    for (name, prompts) in harnx_client::client_prompts() {
        if client == *name && !harnx_client::is_openai_compatible_provider_name(client) {
            return create_config(prompts, name).await;
        }
    }
    if let Some(ret) = create_openai_compatible_client_config(client).await? {
        return Ok(ret);
    }
    bail!("Unknown client '{}'", client)
}

static EMBEDDING_MODEL_RE: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(r"((^|/)(bge-|e5-|uae-|gte-|text-)|embed|multilingual|minilm)").unwrap()
});

async fn set_client_models_config(client_config: &mut Value, client: &str) -> Result<String> {
    if let Some(provider) = harnx_client::ALL_PROVIDER_MODELS
        .iter()
        .find(|v| v.provider == client)
    {
        let models: Vec<String> = provider
            .models
            .iter()
            .filter(|v| v.model_type == "chat")
            .map(|v| v.name.clone())
            .collect();
        let model_name = select_model(models)?;
        return Ok(format!("{client}:{model_name}"));
    }
    let mut model_names = vec![];
    if let (Some(true), Some(api_base), api_key) = (
        client_config["type"]
            .as_str()
            .map(|v| v == harnx_client::OpenAICompatibleClient::NAME),
        client_config["api_base"].as_str(),
        client_config["api_key"]
            .as_str()
            .map(|v| v.to_string())
            .or_else(|| {
                let env_name = format!("{client}_api_key").to_ascii_uppercase();
                std::env::var(&env_name).ok()
            }),
    ) {
        match abortable_run_with_spinner(
            fetch_models(api_base, api_key.as_deref()),
            "Fetching models",
            create_abort_signal(),
        )
        .await
        {
            Ok(fetched_models) => {
                model_names = MultiSelect::new("LLMs to include (required):", fetched_models)
                    .with_validator(|list: &[ListOption<&String>]| {
                        if list.is_empty() {
                            Ok(Validation::Invalid(
                                "At least one item must be selected".into(),
                            ))
                        } else {
                            Ok(Validation::Valid)
                        }
                    })
                    .prompt()?;
            }
            Err(err) => {
                eprintln!("✗ Fetch models failed: {err}");
            }
        }
    }
    if model_names.is_empty() {
        model_names = prompt_input_string(
            "LLMs to add",
            true,
            Some("Separated by commas, e.g. llama3.3,qwen2.5"),
        )?
        .split(',')
        .filter_map(|v| {
            let v = v.trim();
            if v.is_empty() {
                None
            } else {
                Some(v.to_string())
            }
        })
        .collect::<Vec<_>>();
    }
    if model_names.is_empty() {
        bail!("No models");
    }
    let models: Vec<Value> = model_names
        .iter()
        .map(|v| {
            let l = v.to_lowercase();
            if l.contains("rank") {
                json!({
                    "name": v,
                    "type": "reranker",
                })
            } else if let Ok(true) = EMBEDDING_MODEL_RE.is_match(&l) {
                json!({
                    "name": v,
                    "type": "embedding",
                    "default_chunk_size": 1000,
                    "max_batch_size": 100
                })
            } else if v.contains("vision") {
                json!({
                    "name": v,
                    "supports_vision": true
                })
            } else {
                json!({
                    "name": v,
                })
            }
        })
        .collect();
    client_config["models"] = models.into();
    let model_name = select_model(model_names)?;
    Ok(format!("{client}:{model_name}"))
}

fn select_model(model_names: Vec<String>) -> Result<String> {
    if model_names.is_empty() {
        bail!("No models");
    }
    let model = if model_names.len() == 1 {
        model_names[0].clone()
    } else {
        Select::new("Default Model (required):", model_names).prompt()?
    };
    Ok(model)
}

fn prompt_input_string(
    desc: &str,
    required: bool,
    help_message: Option<&str>,
) -> anyhow::Result<String> {
    let desc = if required {
        format!("{desc} (required):")
    } else {
        format!("{desc} (optional):")
    };
    let mut text = Text::new(&desc);
    if required {
        text = text.with_validator(required!("This field is required"))
    }
    if let Some(help_message) = help_message {
        text = text.with_help_message(help_message);
    }
    let text = text.prompt()?;
    Ok(text)
}
