use super::{Session, SessionLogEntry};

use crate::client::MessageContent;
use crate::tool::ToolResult;

use std::collections::HashMap;
use std::path::Path;

pub(crate) fn attachments_dir(session: &Session) -> Option<std::path::PathBuf> {
    let agent_name = session.agent_name.as_deref()?;
    super::Config::session_attachments_dir(super::SessionAttachmentPath {
        agent_name,
        session_id: session.id(),
    })
}

fn externalize_message(
    dir: &Path,
    content: &mut MessageContent,
    map: &mut HashMap<String, String>,
) -> anyhow::Result<()> {
    match content {
        MessageContent::Array(parts) => {
            crate::config::attachments::externalize_parts(dir, parts, map)
        }
        MessageContent::ToolCalls(tool_calls) => {
            for result in &mut tool_calls.tool_results {
                crate::config::attachments::externalize_parts(dir, &mut result.content, map)?;
            }
            Ok(())
        }
        MessageContent::Text(_) => Ok(()),
    }
}

/// Externalize inline image data URIs in a single message content into `cid:`
/// references and return any new `cid -> filename` mappings for later
/// persistence. No-op for non-session-backed runs.
pub(crate) fn externalize_content(
    session: &Session,
    content: &mut MessageContent,
) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Some(dir) = attachments_dir(session) else {
        return map;
    };
    if let Err(err) = externalize_message(&dir, content, &mut map) {
        log::warn!("attachment externalization failed: {err}");
    }
    map
}

/// Externalize inline image data URIs across a slice of tool results into
/// `cid:` references, recording `cid -> filename` in `map`. No-op when the
/// session has no attachments dir yet.
pub(crate) fn externalize_tool_result_content(
    dir: Option<&Path>,
    results: &mut [ToolResult],
    map: &mut HashMap<String, String>,
) {
    let Some(dir) = dir else {
        return;
    };
    for slot in results.iter_mut() {
        if let Err(err) = crate::config::attachments::externalize_parts(dir, &mut slot.content, map)
        {
            log::warn!("tool-result attachment externalization failed: {err}");
        }
    }
}

/// Record freshly externalized `cid -> filename` mappings: append a `DataUrls`
/// log entry and merge them into the session map. No-op when empty. Returns
/// whether the append (if any) succeeded.
pub(crate) fn record_externalized(
    session: &mut Session,
    cid_urls: HashMap<String, String>,
) -> bool {
    if cid_urls.is_empty() {
        return true;
    }
    let appended = crate::config::session::append_event(
        session,
        &SessionLogEntry::DataUrls {
            urls: cid_urls.clone(),
        },
    );
    session.data_urls.extend(cid_urls);
    appended
}
