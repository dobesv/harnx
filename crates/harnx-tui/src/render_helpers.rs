use harnx_core::event::AgentSource;

pub(crate) fn render_status_line(title: Option<&str>, status: Option<&str>) -> Option<String> {
    let line = [title, status]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
    (!line.is_empty()).then_some(format!("-> {line}"))
}

pub(crate) fn source_heading(source: &AgentSource) -> String {
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
    source: Option<&AgentSource>,
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
