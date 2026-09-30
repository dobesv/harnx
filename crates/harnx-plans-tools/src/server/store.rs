use super::params::ReplaceInContent;
use harnx_core::cid_url::{CidUrl, PlanItem, SessionRef};
use rmcp::model::{CallToolResult, ContentBlock, ErrorData};
use serde_json::Value;
use similar::{ChangeTag, TextDiff};

pub(crate) struct BodyEdit {
    pub replace: Option<String>,
    pub append: Option<String>,
    pub replace_in: Option<ReplaceInContent>,
}

pub(crate) fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

pub(crate) fn gen_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:08x}", (nanos & 0xffff_ffff) as u32)
}

pub(crate) fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

pub(crate) fn plan_item_id(url: &str) -> Option<String> {
    match CidUrl::parse(url).ok()? {
        CidUrl::Plan {
            item: PlanItem::Task(id) | PlanItem::Note(id),
            ..
        } => Some(id),
        _ => None,
    }
}

pub(crate) fn slugify(value: &str) -> Result<String, ErrorData> {
    let mut slug = String::new();
    let mut separator = false;
    for byte in value.trim().bytes().map(|byte| byte.to_ascii_lowercase()) {
        if byte.is_ascii_lowercase() || byte.is_ascii_digit() {
            slug.push(char::from(byte));
            separator = false;
        } else if !slug.is_empty() && !separator {
            slug.push('-');
            separator = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        return Err(ErrorData::invalid_params(
            "plan name must contain at least one letter or digit",
            None,
        ));
    }
    Ok(slug)
}

pub(crate) fn validate_item_id(value: &str) -> Result<String, ErrorData> {
    let value = value.trim().to_ascii_lowercase();
    let valid = (1..=64).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if !valid {
        return Err(ErrorData::invalid_params(
            "item ID must match [a-z0-9-] and contain 1-64 characters",
            None,
        ));
    }
    Ok(value)
}

fn parse_url(raw: &str, expected: &str) -> Result<CidUrl, ErrorData> {
    CidUrl::parse(raw).map_err(|error| {
        ErrorData::invalid_params(
            format!("invalid {expected} URL '{raw}': {error}; expected {expected}"),
            None,
        )
    })
}

pub(crate) fn parse_plan_url(raw: &str) -> Result<CidUrl, ErrorData> {
    let url = parse_url(raw, "cid:plan:<agent>/<session-id>/<slug>")?;
    if matches!(
        url,
        CidUrl::Plan {
            item: PlanItem::Index,
            ..
        }
    ) {
        Ok(url)
    } else {
        Err(ErrorData::invalid_params(
            format!("expected plan URL cid:plan:<agent>/<session-id>/<slug>; got '{raw}'"),
            None,
        ))
    }
}

pub(crate) fn parse_task_url(plan: &CidUrl, raw: &str) -> Result<CidUrl, ErrorData> {
    parse_item_url(plan, raw, true)
}

pub(crate) fn parse_note_url(plan: &CidUrl, raw: &str) -> Result<CidUrl, ErrorData> {
    parse_item_url(plan, raw, false)
}

fn parse_item_url(plan: &CidUrl, raw: &str, task: bool) -> Result<CidUrl, ErrorData> {
    let expected = if task {
        "cid:plan:<agent>/<session-id>/<slug>/tasks/<task-id>"
    } else {
        "cid:plan:<agent>/<session-id>/<slug>/notes/<note-id>"
    };
    let url = parse_url(raw, expected)?;
    let valid_item = match &url {
        CidUrl::Plan { item, .. } if task => matches!(item, PlanItem::Task(_)),
        CidUrl::Plan { item, .. } => matches!(item, PlanItem::Note(_)),
        _ => false,
    };
    let belongs_to_plan =
        valid_item && plan.session() == url.session() && plan_slug(plan) == plan_slug(&url);
    if !belongs_to_plan {
        return Err(ErrorData::invalid_params(
            format!("expected {expected} belonging to {plan}; got '{raw}'"),
            None,
        ));
    }
    Ok(url)
}

pub(crate) fn plan_slug(url: &CidUrl) -> &str {
    match url {
        CidUrl::Plan { slug, .. } => slug,
        CidUrl::Media { .. } => "",
    }
}

pub(crate) fn item_url(plan: &CidUrl, item: PlanItem) -> CidUrl {
    CidUrl::Plan {
        session: plan.session().clone(),
        slug: plan_slug(plan).to_string(),
        item,
    }
}

pub(crate) fn session_from_tool(
    session: &harnx_toolset::SessionRef,
) -> Result<SessionRef, ErrorData> {
    SessionRef::new(session.agent.clone(), session.session_id.clone()).map_err(|error| {
        ErrorData::invalid_params(format!("invalid caller session: {error}"), None)
    })
}

pub(crate) fn validate_dependencies(values: Vec<String>) -> Result<Vec<String>, ErrorData> {
    values
        .into_iter()
        .map(|raw| {
            let url = parse_url(&raw, "cid:plan:<agent>/<session-id>/<slug>/tasks/<task-id>")?;
            if matches!(
                url,
                CidUrl::Plan {
                    item: PlanItem::Task(_),
                    ..
                }
            ) {
                Ok(url.to_string())
            } else {
                Err(ErrorData::invalid_params(
                    format!("task dependency must be a task URL; got '{raw}'"),
                    None,
                ))
            }
        })
        .collect()
}

pub(crate) fn apply_body_edit(current: &str, edit: BodyEdit) -> Result<String, ErrorData> {
    let count = [
        edit.replace.is_some(),
        edit.append.is_some(),
        edit.replace_in.is_some(),
    ]
    .into_iter()
    .filter(|provided| *provided)
    .count();
    if count > 1 {
        return Err(ErrorData::invalid_params(
            "provide at most one body edit",
            None,
        ));
    }
    if let Some(replacement) = edit.replace {
        return Ok(replacement);
    }
    if let Some(append) = edit.append {
        let separator = if current.is_empty() || current.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        return Ok(format!("{current}{separator}{append}"));
    }
    if let Some(replace_in) = edit.replace_in {
        return apply_replace_in(current, &replace_in);
    }
    Ok(current.to_string())
}

fn apply_replace_in(body: &str, edit: &ReplaceInContent) -> Result<String, ErrorData> {
    if edit.old_text.is_empty() {
        return Err(ErrorData::invalid_params(
            "old_text must not be empty",
            None,
        ));
    }
    if !body.contains(&edit.old_text) {
        return Err(ErrorData::invalid_params(
            format!("old_text {:?} not found in body", edit.old_text),
            None,
        ));
    }
    if edit.replace_all == Some(true) {
        Ok(body.replace(&edit.old_text, &edit.new_text))
    } else {
        Ok(body.replacen(&edit.old_text, &edit.new_text, 1))
    }
}

pub(crate) fn result_json(value: Value) -> Result<CallToolResult, ErrorData> {
    let text = serde_json::to_string_pretty(&value)
        .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
    result_text(text)
}

pub(crate) fn result_text(text: impl Into<String>) -> Result<CallToolResult, ErrorData> {
    Ok(CallToolResult::success(vec![ContentBlock::text(
        text.into(),
    )]))
}

pub(crate) const PLAN_MARKDOWN_MIME: &str = "text/markdown; charset=utf-8";

pub(crate) struct ResponseResource {
    url: String,
    name: &'static str,
    label: String,
}

impl ResponseResource {
    pub(crate) fn new(url: &CidUrl, name: &'static str, label: impl Into<String>) -> Self {
        Self {
            url: url.to_string(),
            name,
            label: label.into(),
        }
    }
}

pub(crate) fn item_resources(
    plan: &CidUrl,
    item: &CidUrl,
    name: &'static str,
    item_label: &str,
) -> Vec<ResponseResource> {
    vec![
        ResponseResource::new(plan, "plan", "Plan Index"),
        ResponseResource::new(item, name, item_label),
    ]
}

pub(crate) fn result_json_with_resources(
    value: Value,
    resources: Vec<ResponseResource>,
) -> Result<CallToolResult, ErrorData> {
    let text = serde_json::to_string_pretty(&value)
        .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
    result_text_with_resources(text, resources)
}

fn result_text_with_resources(
    text: String,
    resources: Vec<ResponseResource>,
) -> Result<CallToolResult, ErrorData> {
    let mut content = vec![ContentBlock::text(text)];
    if !resources.is_empty() {
        let links = resources
            .iter()
            .map(|resource| format!("[{}]({})", markdown_label(&resource.label), resource.url))
            .collect::<Vec<_>>()
            .join(" · ");
        content.push(ContentBlock::text(links));
    }
    content.extend(resources.into_iter().map(|resource| {
        ContentBlock::resource_link(
            rmcp::model::Resource::new(resource.url, resource.name)
                .with_mime_type(PLAN_MARKDOWN_MIME),
        )
    }));
    Ok(CallToolResult::success(content))
}

fn markdown_label(label: &str) -> String {
    label
        .replace('\\', "\\\\")
        .replace('[', "\\[")
        .replace(']', "\\]")
}

pub(crate) struct DiffResult {
    pub message: String,
    pub diff: String,
}

pub(crate) fn result_with_diff(input: DiffResult) -> Result<CallToolResult, ErrorData> {
    if input.diff.is_empty() {
        result_text(input.message)
    } else {
        result_text(format!("{}\n\n{}", input.message, input.diff))
    }
}

pub(crate) fn result_with_diff_and_resources(
    input: DiffResult,
    resources: Vec<ResponseResource>,
) -> Result<CallToolResult, ErrorData> {
    let text = if input.diff.is_empty() {
        input.message
    } else {
        format!("{}\n\n{}", input.message, input.diff)
    };
    result_text_with_resources(text, resources)
}

pub(crate) struct DiffText<'a> {
    pub before: &'a str,
    pub after: &'a str,
    pub path: &'a str,
}

pub(crate) fn diff_text(input: DiffText<'_>) -> String {
    let DiffText {
        before,
        after,
        path,
    } = input;
    if before == after {
        return String::new();
    }
    let diff = TextDiff::from_lines(before, after);
    let mut output = format!("--- a/{path}\n+++ b/{path}\n");
    for operation in diff.ops() {
        for change in diff.iter_changes(operation) {
            let sign = match change.tag() {
                ChangeTag::Delete => '-',
                ChangeTag::Insert => '+',
                ChangeTag::Equal => ' ',
            };
            output.push(sign);
            output.push_str(change.value());
            if !change.value().ends_with('\n') {
                output.push('\n');
            }
        }
    }
    format!("```diff\n{output}```")
}

pub(crate) fn map_internal(error: anyhow::Error) -> ErrorData {
    ErrorData::internal_error(error.to_string(), None)
}
