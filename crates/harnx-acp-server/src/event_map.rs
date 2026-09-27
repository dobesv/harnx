//! Mapping from harnx's canonical event stream to ACP session updates.

use agent_client_protocol::schema::v1::{
    ContentBlock as AcpContentBlock, ContentChunk, SessionUpdate, TextContent, ToolCall,
    ToolCallContent, ToolCallLocation, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
    ToolKind as AcpToolKind,
};
use harnx_core::api_types::CompletionTokenUsage;
use harnx_core::event::{
    AgentEvent, ContentBlock, ModelEvent, NoticeEvent, SubAgentProgress, SubAgentProgressStatus,
    ToolEvent, ToolKind, ToolLocation, ToolStatus, TurnEvent, UserEvent,
};

use crate::handoff::{committed_target, fallback_update};
use crate::{HARNX_ERROR_META, HARNX_MARKDOWN_META, HARNX_USAGE_META};

struct ToolStart {
    id: String,
    name: String,
    kind: ToolKind,
    markdown: Option<String>,
    input: serde_json::Value,
    locations: Vec<harnx_core::event::ToolLocation>,
}

struct ToolPatch {
    id: String,
    status: Option<ToolCallStatus>,
    markdown: Option<String>,
    content: Option<Vec<ContentBlock>>,
    title: Option<String>,
    kind: Option<ToolKind>,
    locations: Option<Vec<ToolLocation>>,
    usage: Option<CompletionTokenUsage>,
}
/// Convert one harnx event into the ACP update visible to IDE clients.
pub fn agent_event_to_session_update(event: AgentEvent) -> Option<SessionUpdate> {
    agent_event_to_session_update_for_cluster(event, harnx_runtime::config::LOCAL_CLUSTER_KEY)
}

/// Convert an event while resolving bare committed targets against source cluster.
pub fn agent_event_to_session_update_for_cluster(
    event: AgentEvent,
    source_cluster: &str,
) -> Option<SessionUpdate> {
    agent_event_to_update_inner(event, source_cluster, true)
}

/// Convert durable transcript replay events without rendering control records.
pub fn agent_event_to_replay_update(
    event: AgentEvent,
    source_cluster: &str,
) -> Option<SessionUpdate> {
    match event {
        AgentEvent::User(UserEvent::Message { content }) => {
            text_chunk(content, false).map(SessionUpdate::UserMessageChunk)
        }
        AgentEvent::Model(ModelEvent::Final { output, .. }) => {
            text_chunk(output, false).map(SessionUpdate::AgentMessageChunk)
        }
        event => agent_event_to_update_inner(event, source_cluster, false),
    }
}

fn agent_event_to_update_inner(
    event: AgentEvent,
    source_cluster: &str,
    allow_handoff: bool,
) -> Option<SessionUpdate> {
    if allow_handoff {
        if let Some(target) = committed_target(&event, source_cluster) {
            return Some(fallback_update(&target));
        }
    }
    match event {
        AgentEvent::Model(event) => model_event_to_update(event),
        AgentEvent::Tool(event) => Some(tool_event_to_update(event)),
        AgentEvent::Notice(event) => notice_event_to_update(event),
        AgentEvent::SubAgent { event, .. } => {
            agent_event_to_update_inner(*event, source_cluster, false)
        }
        AgentEvent::Turn(TurnEvent::SubAgentProgress(progress)) => {
            Some(subagent_progress_to_update(&progress))
        }
        _ => None,
    }
}

fn model_event_to_update(event: ModelEvent) -> Option<SessionUpdate> {
    match event {
        ModelEvent::MessageChunk { blocks } => text_from_blocks(&blocks)
            .and_then(|text| text_chunk(text, false))
            .map(SessionUpdate::AgentMessageChunk),
        ModelEvent::ThoughtChunk { blocks } => text_from_blocks(&blocks)
            .and_then(|text| text_chunk(text, false))
            .map(SessionUpdate::AgentThoughtChunk),
        ModelEvent::Error(error) => text_chunk(error, true).map(SessionUpdate::AgentMessageChunk),
        ModelEvent::Final { .. } | ModelEvent::Usage { .. } => None,
    }
}

fn tool_event_to_update(event: ToolEvent) -> SessionUpdate {
    match event {
        ToolEvent::Started {
            id,
            name,
            kind,
            markdown,
            input,
            locations,
        } => SessionUpdate::ToolCall(tool_call_started(ToolStart {
            id,
            name,
            kind,
            markdown,
            input,
            locations,
        })),
        ToolEvent::Progress { id, text } => SessionUpdate::ToolCallUpdate(tool_call_update(
            id,
            ToolCallStatus::InProgress,
            non_empty(text),
            None,
        )),
        ToolEvent::Update {
            id,
            markdown,
            status,
            content,
            title,
            kind,
            locations,
            usage,
        } => SessionUpdate::ToolCallUpdate(tool_call_update_with_fields(ToolPatch {
            id,
            status: status.map(map_tool_status),
            markdown,
            content,
            title,
            kind,
            locations,
            usage,
        })),
        ToolEvent::Completed {
            id,
            output,
            markdown,
        } => SessionUpdate::ToolCallUpdate(tool_call_completed(id, output, markdown)),
        ToolEvent::Failed { id, error } => SessionUpdate::ToolCallUpdate(tool_call_update(
            id,
            ToolCallStatus::Failed,
            non_empty(error),
            None,
        )),
        ToolEvent::Blocked {
            id,
            name,
            input,
            reason,
        } => SessionUpdate::ToolCall(blocked_tool_call(id, name, input, reason)),
    }
}

fn tool_call_started(start: ToolStart) -> ToolCall {
    let tool_call_id = if start.id.is_empty() {
        start.name.clone()
    } else {
        start.id
    };
    let mut call = ToolCall::new(tool_call_id, start.name.clone())
        .name(start.name)
        .kind(map_tool_kind(start.kind))
        .raw_input(start.input)
        .locations(
            start
                .locations
                .into_iter()
                .map(|location| ToolCallLocation::new(location.path).line(location.line))
                .collect(),
        );
    if let Some(meta) = markdown_meta(start.markdown) {
        call = call.meta(meta);
    }
    call
}

fn blocked_tool_call(
    id: String,
    name: String,
    input: serde_json::Value,
    reason: String,
) -> ToolCall {
    let tool_call_id = if id.is_empty() { name.clone() } else { id };
    let content = non_empty(reason).map(tool_content).into_iter().collect();
    ToolCall::new(tool_call_id, name.clone())
        .name(name)
        .status(ToolCallStatus::Failed)
        .content(content)
        .raw_input(input)
}

fn tool_call_update(
    id: String,
    status: ToolCallStatus,
    text: Option<String>,
    markdown: Option<String>,
) -> ToolCallUpdate {
    let mut fields = ToolCallUpdateFields::new().status(status);
    if let Some(text) = text {
        fields = fields.content(vec![tool_content(text)]);
    }
    let mut update = ToolCallUpdate::new(id, fields);
    if let Some(meta) = markdown_meta(markdown) {
        update = update.meta(meta);
    }
    update
}

fn tool_call_update_with_fields(patch: ToolPatch) -> ToolCallUpdate {
    let ToolPatch {
        id,
        status,
        markdown,
        content,
        title,
        kind,
        locations,
        usage,
    } = patch;
    let mut fields = ToolCallUpdateFields::new();
    if let Some(status) = status {
        fields = fields.status(status);
    }
    let mapped_content = non_empty_option(markdown.clone())
        .map(|text| vec![tool_content(text)])
        .or_else(|| map_update_content(content));
    if let Some(content) = mapped_content {
        fields = fields.content(content);
    }
    if let Some(title) = title {
        fields = fields.title(title);
    }
    if let Some(kind) = kind {
        fields = fields.kind(map_tool_kind(kind));
    }
    if let Some(locations) = locations {
        fields = fields.locations(
            locations
                .into_iter()
                .map(map_tool_location)
                .collect::<Vec<_>>(),
        );
    }

    let mut meta = markdown_meta(markdown).unwrap_or_default();
    if let Some(usage) = usage {
        meta.insert(
            HARNX_USAGE_META.to_string(),
            serde_json::to_value(usage).expect("completion token usage serializes"),
        );
    }
    let mut update = ToolCallUpdate::new(id, fields);
    if !meta.is_empty() {
        update = update.meta(meta);
    }
    update
}

fn map_update_content(content: Option<Vec<ContentBlock>>) -> Option<Vec<ToolCallContent>> {
    let blocks = content?;
    if blocks.is_empty() {
        return Some(Vec::new());
    }
    text_from_blocks(&blocks).map(|text| vec![tool_content(text)])
}

fn map_tool_location(location: ToolLocation) -> ToolCallLocation {
    ToolCallLocation::new(location.path).line(location.line)
}

fn tool_call_completed(
    id: String,
    output: serde_json::Value,
    markdown: Option<String>,
) -> ToolCallUpdate {
    let text = non_empty_option(markdown.clone()).or_else(|| non_empty(output_text(&output)));
    let mut fields = ToolCallUpdateFields::new()
        .status(ToolCallStatus::Completed)
        .raw_output(output);
    if let Some(text) = text {
        fields = fields.content(vec![tool_content(text)]);
    }
    let mut update = ToolCallUpdate::new(id, fields);
    if let Some(meta) = markdown_meta(markdown) {
        update = update.meta(meta);
    }
    update
}

fn notice_event_to_update(event: NoticeEvent) -> Option<SessionUpdate> {
    let text = match event {
        NoticeEvent::Info(_) => return None,
        NoticeEvent::Warning(message) => non_empty(message).map(|message| format!("⚠ {message}")),
        NoticeEvent::Error(message) => non_empty(message).map(|message| format!("🔴 {message}")),
    }?;
    text_chunk(text, false).map(SessionUpdate::AgentMessageChunk)
}

/// Map subagent progress into an ACP ToolCallUpdate.
///
/// The `invocation_id` field correlates to the parent tool call that started
/// the subagent (resolved at emission time, not session ID). ACP has no native
/// usage field, so structured usage is placed in namespaced `_meta` under
/// `harnx:usage`. Title incorporates the child session title plus compact usage.
///
/// ACP status is limited to {Pending, InProgress, Completed, Failed}. Preserve
/// richer internal states (`Cancelling`, `Cancelled`, `Unconfirmed`) by NOT
/// equating child `Done` with parent tool completion — the parent tool call
/// remains InProgress until the subagent tool completes.
fn subagent_progress_to_update(progress: &SubAgentProgress) -> SessionUpdate {
    let id = progress.invocation_id.clone();

    // Build title: agent name + child title (if present) + compact usage
    let mut title_parts = vec![format!("@ {}", progress.agent)];
    if let Some(ref child_title) = progress.title {
        let trimmed = child_title.trim();
        if !trimmed.is_empty() {
            title_parts.push(trimmed.to_string());
        }
    }
    let usage = &progress.usage;
    if usage.input_tokens > 0 || usage.output_tokens > 0 {
        title_parts.push(format!("({}→{})", usage.input_tokens, usage.output_tokens));
    }
    let title = title_parts.join(" — ");

    // Preserve richer internal states (Cancelling, Cancelled, Unconfirmed).
    // Do NOT equate child Done with parent-tool completion: while child is Done,
    // the parent tool call is still finishing. Parent completes when ToolEvent::Completed arrives.
    let status = match progress.status {
        SubAgentProgressStatus::Running
        | SubAgentProgressStatus::Cancelling
        | SubAgentProgressStatus::Unconfirmed
        | SubAgentProgressStatus::Done => ToolCallStatus::InProgress,
        SubAgentProgressStatus::Cancelled => ToolCallStatus::Failed,
        SubAgentProgressStatus::Failed => ToolCallStatus::Failed,
    };

    // Build fields with title and status
    let fields = ToolCallUpdateFields::new().status(status).title(title);

    // Structured usage under namespaced _meta
    let mut meta = serde_json::Map::new();
    meta.insert(
        HARNX_USAGE_META.to_string(),
        serde_json::to_value(&progress.usage).expect("usage serializes"),
    );

    let update = ToolCallUpdate::new(id, fields).meta(meta);
    SessionUpdate::ToolCallUpdate(update)
}

fn text_chunk(text: String, error: bool) -> Option<ContentChunk> {
    let text = non_empty(text)?;
    let mut chunk = ContentChunk::new(AcpContentBlock::Text(TextContent::new(text)));
    if error {
        let mut meta = serde_json::Map::new();
        meta.insert(HARNX_ERROR_META.to_string(), serde_json::Value::Bool(true));
        chunk = chunk.meta(meta);
    }
    Some(chunk)
}

fn tool_content(text: String) -> ToolCallContent {
    AcpContentBlock::Text(TextContent::new(text)).into()
}

fn text_from_blocks(blocks: &[ContentBlock]) -> Option<String> {
    let text = blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    non_empty(text)
}

fn output_text(output: &serde_json::Value) -> String {
    output
        .as_str()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| output.to_string())
}

fn markdown_meta(markdown: Option<String>) -> Option<serde_json::Map<String, serde_json::Value>> {
    let markdown = non_empty_option(markdown)?;
    let mut meta = serde_json::Map::new();
    meta.insert(
        HARNX_MARKDOWN_META.to_string(),
        serde_json::Value::String(markdown),
    );
    Some(meta)
}

fn non_empty(text: String) -> Option<String> {
    (!text.is_empty()).then_some(text)
}

fn non_empty_option(text: Option<String>) -> Option<String> {
    text.and_then(non_empty)
}

fn map_tool_kind(kind: ToolKind) -> AcpToolKind {
    match kind {
        ToolKind::Read => AcpToolKind::Read,
        ToolKind::Edit => AcpToolKind::Edit,
        ToolKind::Delete => AcpToolKind::Delete,
        ToolKind::Move => AcpToolKind::Move,
        ToolKind::Search => AcpToolKind::Search,
        ToolKind::Execute => AcpToolKind::Execute,
        ToolKind::Think => AcpToolKind::Think,
        ToolKind::Fetch => AcpToolKind::Fetch,
        ToolKind::SwitchMode => AcpToolKind::SwitchMode,
        ToolKind::Other => AcpToolKind::Other,
    }
}

pub fn map_tool_status(status: ToolStatus) -> ToolCallStatus {
    match status {
        ToolStatus::Pending => ToolCallStatus::Pending,
        ToolStatus::InProgress => ToolCallStatus::InProgress,
        ToolStatus::Completed => ToolCallStatus::Completed,
        ToolStatus::Failed => ToolCallStatus::Failed,
    }
}
