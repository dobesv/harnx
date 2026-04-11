use crate::ui_output::{UiOutputEventKind, UiOutputSource};

pub(crate) fn render_status_line(title: Option<&str>, status: Option<&str>) -> Option<String> {
    let line = [title, status]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
    (!line.is_empty()).then_some(format!("-> {line}"))
}

pub(crate) fn source_heading(source: &UiOutputSource) -> String {
    match &source.session_id {
        Some(session_id) if !session_id.is_empty() => {
            format!("> {} ▸ {}", source.agent, session_id)
        }
        _ => format!("> {}", source.agent),
    }
}

pub(crate) fn render_usage_line(
    input_tokens: u64,
    output_tokens: u64,
    cached_tokens: u64,
    session_label: Option<&str>,
    source: Option<&UiOutputSource>,
) -> Option<String> {
    let mut parts = vec![];
    if let Some(label) = session_label {
        parts.push(label.to_string());
    } else if let Some(source) = source {
        parts.push(source_heading(source));
    }
    if input_tokens > 0 {
        parts.push(format!("in {input_tokens}"));
    }
    if output_tokens > 0 {
        parts.push(format!("out {output_tokens}"));
    }
    if cached_tokens > 0 {
        parts.push(format!("cache {cached_tokens}"));
    }
    (!parts.is_empty()).then(|| parts.join("   "))
}

pub(crate) fn event_fallback_text(
    kind: &UiOutputEventKind,
    source: Option<&UiOutputSource>,
) -> String {
    match kind {
        UiOutputEventKind::MessageChunk { text, .. }
        | UiOutputEventKind::TranscriptText { text }
        | UiOutputEventKind::ToolResultText { text } => text.clone(),
        UiOutputEventKind::LlmFinal { output, usage: _ } => output.clone(),
        UiOutputEventKind::LlmError(err) => err.clone(),
        UiOutputEventKind::ThoughtChunk { text, .. } => {
            format!("<think>{text}</think>")
        }
        UiOutputEventKind::ToolCallUpdate { title, status, .. } => {
            render_status_line(title.as_deref(), status.as_deref())
                .map(|text| format!("\n{text}\n"))
                .unwrap_or_default()
        }
        UiOutputEventKind::Plan { entries } => {
            if entries.is_empty() {
                String::new()
            } else {
                let body = entries
                    .iter()
                    .map(|entry| format!("  [{}] {}", entry.status, entry.content))
                    .collect::<Vec<_>>()
                    .join("\n");
                format!("\nPlan:\n{body}\n")
            }
        }
        UiOutputEventKind::Usage {
            input_tokens,
            output_tokens,
            cached_tokens,
            session_label,
        } => render_usage_line(
            *input_tokens,
            *output_tokens,
            *cached_tokens,
            session_label.as_deref(),
            source,
        )
        .map(|line| format!("\n{line}\n"))
        .unwrap_or_default(),
        UiOutputEventKind::ToolCall {
            tool_name,
            input_yaml,
            ..
        } => {
            let mut lines = vec![format!("->️ {tool_name}")];
            if let Some(input_yaml) = input_yaml {
                lines.extend(input_yaml.lines().map(|line| format!("   {line}")));
            }
            format!("\n{}\n", lines.join("\n"))
        }
    }
}
