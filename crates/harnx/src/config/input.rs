use super::*;

use crate::client::{
    init_client, patch_messages, ChatCompletionsData, Client, Message, MessageContent, MessageRole,
    Model,
};
use crate::utils::{create_abort_signal, is_loader_protocol, AbortSignal};

pub use harnx_core::input::{resolve_data_url, Input};

use anyhow::{bail, Context, Result};
use indexmap::IndexSet;
use std::{collections::HashMap, fs::File, io::Read};

const IMAGE_EXTS: [&str; 5] = ["png", "jpeg", "jpg", "webp", "gif"];

pub fn from_str(config: &GlobalConfig, text: &str, agent: Option<Agent>) -> Input {
    let (agent, with_session, with_agent) = resolve_agent(&config.read(), agent);
    let mut input = Input::new(
        text.to_string(),
        (text.to_string(), vec![]),
        agent.into_config(),
    );
    input.with_session = with_session;
    input.with_agent = with_agent;
    input
}

pub async fn from_files(
    config: &GlobalConfig,
    raw_text: &str,
    paths: Vec<String>,
    agent: Option<Agent>,
) -> Result<Input> {
    let loaders = config.read().document_loaders.clone();
    let (raw_paths, local_paths, remote_urls, external_cmds, protocol_paths, with_last_reply) =
        resolve_paths(&loaders, paths)?;
    let mut last_reply = None;
    let (documents, medias, data_urls) = load_documents(
        &loaders,
        local_paths,
        remote_urls,
        external_cmds,
        protocol_paths,
    )
    .await
    .context("Failed to load files")?;
    let mut texts = vec![];
    if !raw_text.is_empty() {
        texts.push(raw_text.to_string());
    };
    if with_last_reply {
        if let Some(LastMessage { input, output, .. }) = config.read().last_message.as_ref() {
            if !output.is_empty() {
                last_reply = Some(output.clone())
            } else if let Some(v) = input.last_reply.as_ref() {
                last_reply = Some(v.clone());
            }
            if let Some(v) = last_reply.clone() {
                texts.push(format!("\n{v}"));
            }
        }
        if last_reply.is_none() && documents.is_empty() && medias.is_empty() {
            bail!("No last reply found");
        }
    }
    let documents_len = documents.len();
    for (kind, path, contents) in documents {
        if documents_len == 1 && raw_text.is_empty() {
            texts.push(format!("\n{contents}"));
        } else {
            texts.push(format!(
                "\n============ {kind}: {path} ============\n{contents}"
            ));
        }
    }
    let (agent, with_session, with_agent) = resolve_agent(&config.read(), agent);
    let mut input = Input::new(
        texts.join("\n"),
        (raw_text.to_string(), raw_paths),
        agent.into_config(),
    );
    input.last_reply = last_reply;
    input.medias = medias;
    input.data_urls = data_urls;
    input.with_session = with_session;
    input.with_agent = with_agent;
    Ok(input)
}

pub async fn from_files_with_spinner(
    config: &GlobalConfig,
    raw_text: &str,
    paths: Vec<String>,
    agent: Option<Agent>,
    abort_signal: AbortSignal,
) -> Result<Input> {
    abortable_run_with_spinner(
        from_files(config, raw_text, paths, agent),
        "Loading files",
        abort_signal,
    )
    .await
}

pub fn stream(input: &Input, config: &GlobalConfig) -> bool {
    config.read().stream && !input.agent().model().no_stream()
}

pub fn set_regenerate(input: &mut Input, config: &GlobalConfig) {
    let agent = config.read().extract_agent();
    if agent.name() == input.agent().name() {
        input.agent = agent.into_config();
    }
    input.regenerate = true;
    input.tool_calls = None;
}

pub async fn use_embeddings(
    input: &mut Input,
    config: &GlobalConfig,
    abort_signal: AbortSignal,
) -> Result<()> {
    if input.raw_text().is_empty() {
        return Ok(());
    }
    let rag = config.read().rag.clone();
    if let Some(rag) = rag {
        let result = Config::search_rag(config, &rag, input.raw_text(), abort_signal).await?;
        input.set_patched_text(Some(result));
        input.rag_name = Some(rag.name().to_string());
    }
    Ok(())
}

pub fn create_client(input: &Input, config: &GlobalConfig) -> Result<Box<dyn Client>> {
    init_client(&config.read().clients, input.agent().model())
}

/// Fetch chat text with retry and model fallback support.
/// Uses the agent's configured fallback models if the primary model fails.
pub async fn fetch_chat_text(input: &Input, config: &GlobalConfig) -> Result<String> {
    let abort_signal = create_abort_signal();
    let (text, _, _, _) =
        crate::client::retry::call_with_retry_and_fallback(input, config, abort_signal).await?;
    let text = strip_think_tag(&text).to_string();
    Ok(text)
}

pub fn prepare_completion_data(
    input: &Input,
    config: &GlobalConfig,
    model: &Model,
    stream: bool,
) -> Result<ChatCompletionsData> {
    let mut messages = build_messages(input, config)?;
    patch_messages(&mut messages, model);
    model.guard_max_input_tokens(&messages)?;
    let (temperature, top_p) = (input.agent().temperature(), input.agent().top_p());
    let functions = config.read().select_tools(input.agent());
    Ok(ChatCompletionsData {
        messages,
        temperature,
        top_p,
        functions,
        stream,
    })
}

pub fn build_messages(input: &Input, config: &GlobalConfig) -> Result<Vec<Message>> {
    let mut messages = if let Some(session) = session_of(input, &config.read().session) {
        crate::config::session::build_messages(session, input)
    } else {
        crate::config::agent::build_messages(input.agent(), input)
    };
    if let Some(tool_calls) = &input.tool_calls {
        messages.push(Message::new(
            MessageRole::Assistant,
            MessageContent::ToolCalls(tool_calls.clone()),
        ))
    }
    if let Some(text) = &input.injected_user_text {
        messages.push(Message::new(
            MessageRole::User,
            MessageContent::Text(text.clone()),
        ))
    }
    Ok(messages)
}

pub fn echo_messages(input: &Input, config: &GlobalConfig) -> String {
    if let Some(session) = session_of(input, &config.read().session) {
        crate::config::session::echo_messages(session, input)
    } else {
        crate::config::agent::echo_messages(input.agent(), input)
    }
}

/// Returns the session to use for this input, if any. Replaces the
/// previous `Input::session(&self, &Option<Session>)` method — kept in
/// harnx because `Session` is a harnx-only type.
pub fn session_of<'a>(input: &Input, session: &'a Option<Session>) -> Option<&'a Session> {
    if input.with_session() {
        session.as_ref()
    } else {
        None
    }
}

pub fn set_agent(input: &mut Input, config: &GlobalConfig, agent: AgentConfig) {
    input.with_agent = !agent.name().trim().is_empty();
    input.with_session = input.with_session || config.read().session.is_some();
    input.inject_system_prompt = true;
    input.agent = agent;
}

fn resolve_agent(config: &Config, agent: Option<Agent>) -> (Agent, bool, bool) {
    match agent {
        Some(v) => (v, false, false),
        None => (
            config.extract_agent(),
            config.session.is_some(),
            config.agent.is_some(),
        ),
    }
}

type ResolvePathsOutput = (
    Vec<String>,
    Vec<String>,
    Vec<String>,
    Vec<String>,
    Vec<String>,
    bool,
);

fn resolve_paths(
    loaders: &HashMap<String, String>,
    paths: Vec<String>,
) -> Result<ResolvePathsOutput> {
    let mut raw_paths = IndexSet::new();
    let mut local_paths = IndexSet::new();
    let mut remote_urls = IndexSet::new();
    let mut external_cmds = IndexSet::new();
    let mut protocol_paths = IndexSet::new();
    let mut with_last_reply = false;
    for path in paths {
        if path == "%%" {
            with_last_reply = true;
            raw_paths.insert(path);
        } else if path.starts_with('`') && path.len() > 2 && path.ends_with('`') {
            external_cmds.insert(path[1..path.len() - 1].to_string());
            raw_paths.insert(path);
        } else if is_url(&path) {
            if path.strip_suffix("**").is_some() {
                bail!("Invalid website '{path}'");
            }
            remote_urls.insert(path.clone());
            raw_paths.insert(path);
        } else if is_loader_protocol(loaders, &path) {
            protocol_paths.insert(path.clone());
            raw_paths.insert(path);
        } else {
            let resolved_path = resolve_home_dir(&path);
            let absolute_path = to_absolute_path(&resolved_path)
                .with_context(|| format!("Invalid path '{path}'"))?;
            local_paths.insert(resolved_path);
            raw_paths.insert(absolute_path);
        }
    }
    Ok((
        raw_paths.into_iter().collect(),
        local_paths.into_iter().collect(),
        remote_urls.into_iter().collect(),
        external_cmds.into_iter().collect(),
        protocol_paths.into_iter().collect(),
        with_last_reply,
    ))
}

async fn load_documents(
    loaders: &HashMap<String, String>,
    local_paths: Vec<String>,
    remote_urls: Vec<String>,
    external_cmds: Vec<String>,
    protocol_paths: Vec<String>,
) -> Result<(
    Vec<(&'static str, String, String)>,
    Vec<String>,
    HashMap<String, String>,
)> {
    let mut files = vec![];
    let mut medias = vec![];
    let mut data_urls = HashMap::new();

    for cmd in external_cmds {
        let output = duct::cmd(&SHELL.cmd, &[&SHELL.arg, &cmd])
            .stderr_to_stdout()
            .unchecked()
            .read()
            .unwrap_or_else(|err| err.to_string());
        files.push(("CMD", cmd, output));
    }

    let local_files = expand_glob_paths(&local_paths, true).await?;
    for file_path in local_files {
        if is_image(&file_path) {
            let contents = read_media_to_data_url(&file_path)
                .with_context(|| format!("Unable to read media '{file_path}'"))?;
            data_urls.insert(sha256(&contents), file_path);
            medias.push(contents)
        } else {
            let document = load_file(loaders, &file_path)
                .await
                .with_context(|| format!("Unable to read file '{file_path}'"))?;
            files.push(("FILE", file_path, document.contents));
        }
    }

    for file_url in remote_urls {
        let (contents, extension) = fetch_with_loaders(loaders, &file_url, true)
            .await
            .with_context(|| format!("Failed to load url '{file_url}'"))?;
        if extension == MEDIA_URL_EXTENSION {
            data_urls.insert(sha256(&contents), file_url);
            medias.push(contents)
        } else {
            files.push(("URL", file_url, contents));
        }
    }

    for protocol_path in protocol_paths {
        let documents = load_protocol_path(loaders, &protocol_path)
            .with_context(|| format!("Failed to load from '{protocol_path}'"))?;
        files.extend(
            documents
                .into_iter()
                .map(|document| ("FROM", document.path, document.contents)),
        );
    }

    Ok((files, medias, data_urls))
}

fn is_image(path: &str) -> bool {
    get_patch_extension(path)
        .map(|v| IMAGE_EXTS.contains(&v.as_str()))
        .unwrap_or_default()
}

fn read_media_to_data_url(image_path: &str) -> Result<String> {
    let extension = get_patch_extension(image_path).unwrap_or_default();
    let mime_type = match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => bail!("Unexpected media type"),
    };
    let mut file = File::open(image_path)?;
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)?;

    let encoded_image = base64_encode(buffer);
    let data_url = format!("data:{mime_type};base64,{encoded_image}");

    Ok(data_url)
}
