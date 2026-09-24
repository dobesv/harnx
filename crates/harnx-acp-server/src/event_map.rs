//! Mapping from harnx's canonical event stream to ACP session updates.

use agent_client_protocol::schema::v1::{
    ContentBlock as AcpContentBlock, ContentChunk, SessionUpdate, TextContent, ToolCall,
    ToolCallContent, ToolCallLocation, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
    ToolKind as AcpToolKind,
};
use harnx_core::event::{
    AgentEvent, ContentBlock, ModelEvent, NoticeEvent, ToolEvent, ToolKind, ToolStatus,
};

use crate::handoff::{committed_target, fallback_update};
use crate::{HARNX_ERROR_META, HARNX_MARKDOWN_META};

struct ToolStart {
    id: String,
    name: String,
    kind: ToolKind,
    markdown: Option<String>,
    input: serde_json::Value,
    locations: Vec<harnx_core::event::ToolLocation>,
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
        } => {
            let text = non_empty_option(markdown.clone())
                .or_else(|| content.as_deref().and_then(text_from_blocks));
            let status = status
                .map(map_tool_status)
                .unwrap_or(ToolCallStatus::InProgress);
            SessionUpdate::ToolCallUpdate(tool_call_update(id, status, text, markdown))
        }
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
