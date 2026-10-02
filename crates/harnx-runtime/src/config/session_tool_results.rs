//! Persistence for completed tool-call rounds.

use super::session_externalize::{
    attachments_dir, externalize_tool_result_content, record_externalized, session_ref,
};
use super::session_persistence::{
    append_event, require_authoritative_appends, PendingExecutionContextPersistence,
};
use crate::client::{MessageContent, MessageRole};
use crate::tool::ToolResult;
use anyhow::Result;
use chrono::Utc;
use harnx_core::execution_context::ExecutionContextObservation;
use harnx_core::session::{Session, SessionLogEntry, ToolOutput};

/// Finalize the tool round opened by [`super::session::add_tool_calls`] by filling in
/// the in-memory outputs and writing a `ToolResults` log entry.
/// Matches each result to its call by id (or by position when the id
/// is absent).
pub async fn add_tool_results(session: &mut Session, results: &[ToolResult]) -> Result<()> {
    let persistence = prepare_tool_results(session, results)?;
    persistence.persist().await;
    Ok(())
}

pub(crate) fn prepare_tool_results(
    session: &mut Session,
    results: &[ToolResult],
) -> Result<PendingExecutionContextPersistence> {
    let mut results = results.to_vec();
    prepare_tool_results_in_place(session, &mut results)
}

/// Prepare tool results for persistence, mutating results in place.
///
/// Externalizes image data URIs to canonical `cid:media:` URLs and inserts
/// adjacent `[attachment: ...]` markers. The returned `ToolResult` array and
/// the persisted `SessionLogEntry::ToolResults` contain identical canonical
/// references — the model sees the same URLs on subsequent turns.
pub(crate) fn prepare_tool_results_in_place(
    session: &mut Session,
    results: &mut [ToolResult],
) -> Result<PendingExecutionContextPersistence> {
    let cid_urls = externalize_results(session, results)?;

    let Some(last) = session.messages.last_mut() else {
        anyhow::bail!("add_tool_results called on empty session");
    };
    let MessageContent::ToolCalls(ref mut pending) = last.content else {
        anyhow::bail!(
            "add_tool_results called but the last session message is not a pending tool-call turn"
        );
    };
    if last.role != MessageRole::Tool {
        anyhow::bail!("add_tool_results called but the last session message is not role=Tool");
    }

    let accepted_observations = accept_tool_result_replacements(&mut pending.tool_results, results);
    let log_results = tool_outputs(&pending.tool_results);

    let appended = append_event(
        session,
        &SessionLogEntry::ToolResults {
            results: log_results,
            timestamp: Some(Utc::now()),
        },
    );
    let all_appended = appended & record_externalized(session, cid_urls);
    session.dirty |= !all_appended;
    require_authoritative_appends(session, all_appended, "tool results")?;
    let persistence = PendingExecutionContextPersistence::for_session(
        session,
        accepted_observations,
        all_appended,
    );
    session.update_tokens();
    Ok(persistence)
}

fn externalize_results(
    session: &Session,
    results: &mut [ToolResult],
) -> Result<std::collections::HashMap<String, String>> {
    let mut cid_urls = std::collections::HashMap::new();
    let Some(attachments_dir) = attachments_dir(session) else {
        return Ok(cid_urls);
    };
    let attachment_session = match session_ref(session) {
        Ok(session_ref) => session_ref,
        Err(err) => {
            log::warn!("tool-result attachment externalization skipped: {err}");
            return Ok(cid_urls);
        }
    };
    externalize_tool_result_content(
        Some(&attachments_dir),
        &attachment_session,
        results,
        &mut cid_urls,
    );
    Ok(cid_urls)
}

fn tool_outputs(results: &[ToolResult]) -> Vec<ToolOutput> {
    results
        .iter()
        .map(|result| ToolOutput {
            id: result.call.id.clone(),
            name: result.call.name.clone(),
            output: result.output.clone(),
            markdown: result.markdown.clone(),
            content: result.content.clone(),
            switch_agent: result.switch_agent.clone(),
        })
        .collect()
}

fn accept_tool_result_replacements(
    pending_results: &mut [ToolResult],
    results: &[ToolResult],
) -> Vec<ExecutionContextObservation> {
    // Match results to the pending calls by id (fallback: position).
    let mut by_id: std::collections::HashMap<String, ToolResult> = results
        .iter()
        .filter_map(|result| result.call.id.clone().map(|id| (id, result.clone())))
        .collect();
    let mut positional = results
        .iter()
        .filter(|result| result.call.id.is_none())
        .cloned();
    let mut accepted_observations = Vec::new();
    for slot in pending_results {
        let replacement = slot
            .call
            .id
            .as_ref()
            .and_then(|id| by_id.remove(id))
            .or_else(|| positional.next());
        if let Some(replacement) = replacement {
            if let Some(observation) = replacement.execution_context {
                accepted_observations.push(observation);
            }
            slot.output = replacement.output;
            slot.content = replacement.content;
            slot.switch_agent = replacement.switch_agent;
        }
    }
    accepted_observations
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::session::{add_tool_calls, new, SessionAppendSink};
    use crate::config::Config;
    use harnx_core::execution_context::ToolObservationProvenance;
    use harnx_core::tool::ToolCall;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct ContextMetadataSink {
        next_seq: AtomicU64,
        context_writes: AtomicUsize,
        contexts: Mutex<Vec<ExecutionContextObservation>>,
        fail: bool,
    }

    impl ContextMetadataSink {
        fn failing() -> Self {
            Self {
                fail: true,
                ..Self::default()
            }
        }
    }

    impl SessionAppendSink for ContextMetadataSink {
        fn append(&self, _entry: &SessionLogEntry) -> Result<u64> {
            Ok(self.next_seq.fetch_add(1, Ordering::SeqCst) + 1)
        }

        fn persist_execution_contexts<'a>(
            &'a self,
            observations: &'a [ExecutionContextObservation],
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
            Box::pin(async move {
                self.context_writes.fetch_add(1, Ordering::SeqCst);
                self.contexts
                    .lock()
                    .expect("captured contexts poisoned")
                    .extend_from_slice(observations);
                if self.fail {
                    anyhow::bail!("simulated execution-context metadata failure");
                }
                Ok(())
            })
        }

        fn failure_is_fatal(&self) -> bool {
            true
        }
    }

    fn setup_session(id: &str, sink: &Arc<ContextMetadataSink>) -> (Session, crate::config::Input) {
        let config = Config::default();
        let global_config = Arc::new(crate::config::ConfigLock::new(config.clone()));
        let input = crate::config::input::from_str(
            &global_config,
            "inspect repository",
            Some(config.extract_agent()),
        );
        let mut session = new(&config, id, None).unwrap();
        session.runtime = Some(Arc::new(sink.clone() as Arc<dyn SessionAppendSink>));
        (session, input)
    }

    fn tool_call(id: &str) -> ToolCall {
        ToolCall::new(
            "fs_read".to_string(),
            json!({"path": "/workspace/README.md"}),
            Some(id.to_string()),
            None,
        )
    }

    fn result_with_context(call: ToolCall, provenance_call_id: &str) -> ToolResult {
        let mut result = ToolResult::new(call, json!({"ok": provenance_call_id}));
        let mut observation = ExecutionContextObservation::observe(
            std::path::Path::new("/workspace"),
            std::path::Path::new("/workspace/README.md"),
        );
        observation.provenance = Some(ToolObservationProvenance::new(
            "scope",
            "fs",
            "read",
            provenance_call_id,
        ));
        result.execution_context = Some(observation);
        result
    }

    #[tokio::test]
    async fn context_metadata_failure_does_not_fail_durable_tool_result() {
        let sink = Arc::new(ContextMetadataSink::failing());
        let (mut session, input) = setup_session("context-metadata-failure", &sink);
        let call = tool_call("call-1");
        add_tool_calls(
            &mut session,
            &input,
            "reading",
            None,
            std::slice::from_ref(&call),
        )
        .unwrap();
        let result = result_with_context(call, "call-1");

        add_tool_results(&mut session, &[result])
            .await
            .expect("durable result must survive context metadata failure");
        assert_eq!(sink.context_writes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn context_metadata_only_uses_results_accepted_for_pending_calls() {
        let sink = Arc::new(ContextMetadataSink::default());
        let (mut session, input) = setup_session("matched-contexts", &sink);
        let pending_call = tool_call("call-1");
        add_tool_calls(
            &mut session,
            &input,
            "reading",
            None,
            std::slice::from_ref(&pending_call),
        )
        .unwrap();

        let result = |id: &str, provenance_call_id: &str| {
            result_with_context(
                ToolCall {
                    id: Some(id.to_string()),
                    ..pending_call.clone()
                },
                provenance_call_id,
            )
        };

        add_tool_results(
            &mut session,
            &[
                result("call-1", "duplicate-discarded"),
                result("unmatched", "unmatched-discarded"),
                result("call-1", "accepted"),
            ],
        )
        .await
        .unwrap();

        let contexts = sink.contexts.lock().expect("captured contexts poisoned");
        assert_eq!(contexts.len(), 1);
        assert_eq!(
            contexts[0]
                .provenance
                .as_ref()
                .map(|provenance| provenance.call_id.as_str()),
            Some("accepted")
        );
    }

    #[tokio::test]
    async fn tool_result_externalization_returns_visible_canonical_marker() -> Result<()> {
        let _lock = super::super::test_support::env_lock_async().await;
        let data_dir = tempfile::tempdir()?;
        let _data_guard =
            super::super::test_support::EnvGuard::new("HARNX_DATA_DIR", data_dir.path());
        let sink = Arc::new(ContextMetadataSink::default());
        let (mut session, input) = setup_session("media1", &sink);
        let call = tool_call("call-media");
        add_tool_calls(
            &mut session,
            &input,
            "reading image",
            None,
            std::slice::from_ref(&call),
        )?;
        let mut result = ToolResult::new(call, json!({"ok": true}));
        result
            .content
            .push(harnx_core::message::MessageContentPart::ImageUrl {
                image_url: harnx_core::message::ImageUrl {
                    url: "data:image/png;base64,QUJD".to_string(),
                },
            });

        let persistence =
            prepare_tool_results_in_place(&mut session, std::slice::from_mut(&mut result))?;
        persistence.persist().await;

        let cid = harnx_core::attachments::cid_for_data_url_with_session(
            &harnx_core::cid_url::SessionRef::new(None, "media1".to_string())?,
            "data:image/png;base64,QUJD",
        );
        assert_eq!(
            result.content,
            vec![
                harnx_core::message::MessageContentPart::ImageUrl {
                    image_url: harnx_core::message::ImageUrl { url: cid.clone() }
                },
                harnx_core::message::MessageContentPart::Text {
                    text: format!("[attachment: {cid} (image/png, 3B)]")
                }
            ]
        );
        let MessageContent::ToolCalls(pending) = &session.messages.last().unwrap().content else {
            unreachable!("tool result remains in pending tool message")
        };
        assert_eq!(pending.tool_results[0].content, result.content);
        Ok(())
    }
}
