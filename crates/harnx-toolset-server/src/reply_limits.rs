//! Terminal-output bounds, independent of the MCP bridge's presentation cap.
use anyhow::{bail, Context, Result};
use async_nats::jetstream;
use base64::Engine;
use harnx_core::{attachments::cid_for_data_url_with_session, cid_url::SessionRef};
use harnx_toolset::{ToolErrorPayload, ToolReply, ToolRequest};

/// Keep the original until a bounded completion wins its journal CAS. A retry
/// can have less room after a concurrent checkpoint/partial-result update.
pub(crate) struct PendingReply {
    candidate: ToolReply,
    original_bytes: Vec<u8>,
    wire_bytes: usize,
    retained: Option<Option<String>>,
}

impl PendingReply {
    pub(crate) fn new(candidate: ToolReply, original_bytes: Vec<u8>) -> Result<Self> {
        let wire_bytes = encoded_len(&candidate)?;
        Ok(Self {
            candidate,
            original_bytes,
            wire_bytes,
            retained: None,
        })
    }

    pub(crate) async fn bounded(
        &mut self,
        attachments: &ReplyAttachments<'_>,
        budget: usize,
        force_size_error: bool,
    ) -> Result<ToolReply> {
        if !force_size_error && self.wire_bytes <= budget {
            return Ok(self.candidate.clone());
        }
        if self.retained.is_none() {
            self.retained = Some(attachments.retain(&self.original_bytes).await?);
        }
        self.size_error_within(budget)
    }

    fn size_error_within(&self, budget: usize) -> Result<ToolReply> {
        let retained = self.retained.as_ref().and_then(Option::as_deref);
        let message = full_size_error_message(self.original_bytes.len(), retained);
        let mut reply = self.size_error(message);
        if encoded_len(&reply)? <= budget {
            return Ok(reply);
        }
        // Progress is advisory, but still part of the budget. Its full snapshot
        // is retained with the original instead of crowding out the terminal.
        reply.final_progress = None;
        if encoded_len(&reply)? <= budget {
            return Ok(reply);
        }
        if let Some(url) = retained {
            reply = self.size_error(short_size_error_message(url));
            reply.final_progress = None;
            if encoded_len(&reply)? <= budget {
                return Ok(reply);
            }
        }
        reply = self.size_error(minimal_size_error_message(retained.is_some()));
        reply.final_progress = None;
        if encoded_len(&reply)? <= budget {
            return Ok(reply);
        }
        // Input/checkpoint headroom is deliberately not an admission policy here.
        // A result-only guard cannot discard those fields to manufacture success.
        bail!("bounded terminal tool reply cannot fit durable completion budget ({budget} bytes)")
    }

    fn size_error(&self, message: String) -> ToolReply {
        let error = match &self.candidate.result {
            Err(ToolErrorPayload::Fatal(_)) => ToolErrorPayload::Fatal(message),
            Err(ToolErrorPayload::Interrupted(interrupted)) => {
                let mut interrupted = interrupted.clone();
                interrupted.reason = message;
                ToolErrorPayload::Interrupted(interrupted)
            }
            Ok(_) | Err(ToolErrorPayload::Recoverable(_)) => ToolErrorPayload::Recoverable(message),
        };
        ToolReply {
            call_id: self.candidate.call_id.clone(),
            result: Err(error),
            final_progress: self.candidate.final_progress.clone(),
        }
    }
}

/// Attachment retention for a trusted session owner. Tool servers derive the
/// owner from the transport-validated request identity; the worker derives
/// it from the session's canonical metadata.
pub struct SessionAttachments<'a> {
    owner: Option<SessionRef>,
    js: &'a jetstream::Context,
    replicas: usize,
}

impl<'a> SessionAttachments<'a> {
    pub fn for_owner(
        owner: Option<SessionRef>,
        js: &'a jetstream::Context,
        replicas: usize,
    ) -> Self {
        Self {
            owner,
            js,
            replicas,
        }
    }

    /// Retain `original_bytes` as a `cid:media:` attachment and return its URL.
    /// `None` means no trusted owner was attached, so nothing was stored.
    pub async fn retain(&self, original_bytes: &[u8]) -> Result<Option<String>> {
        let Some(session) = &self.owner else {
            return Ok(None);
        };
        let store = harnx_blob_store::create_or_open_attachments_bucket(self.js, self.replicas)
            .await
            .context("open tool reply attachment storage")?;
        let data_url = format!(
            "data:application/json;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(original_bytes)
        );
        let url =
            harnx_core::cid_url::CidUrl::parse(&cid_for_data_url_with_session(session, &data_url))?;
        harnx_blob_store::media::put_media_with_chunk_limit(
            &store,
            harnx_blob_store::media::MediaBlob {
                url: &url,
                bytes: original_bytes,
                mime_type: "application/json",
            },
            self.js.client().server_info().max_payload,
        )
        .await
        .context("retain oversized tool reply")?;
        let _ = harnx_blob_store::touch_activity(self.js, &session.owner()).await;
        Ok(Some(url.to_string()))
    }
}

/// The full recoverable size-error message. Shared by the tool-server reply
/// bound and the worker's `ToolResults` session-log bound (#2555), so the
/// model sees the same text and the same `attachment_read` recovery path in
/// both failure modes.
pub fn full_size_error_message(serialized_bytes: usize, retained_url: Option<&str>) -> String {
    let location = match retained_url {
        Some(url) => format!("Full original ToolReply (application/json): {url}. Read with attachment_read using byte_offset/byte_limit."),
        None => "Original was not retained: trusted session attachment storage is unavailable.".into(),
    };
    format!(
        "Tool reply exceeded storage/transport size limit ({serialized_bytes} serialized bytes). {location} Tool side effects may already have occurred; narrow the request before retrying."
    )
}

/// Shortened size-error message used when even the full message does not fit.
pub fn short_size_error_message(retained_url: &str) -> String {
    format!("Tool reply too large. Original: {retained_url}. Side effects may have occurred.")
}

/// Smallest size-error message, used when the `cid:media:` reference itself
/// does not fit. `retained` says whether the original was stored anyway.
pub fn minimal_size_error_message(retained: bool) -> String {
    let location = if retained {
        "Original retained in session attachments; reference does not fit."
    } else {
        "Original not retained; no trusted attachment owner."
    };
    format!("Tool reply too large. {location} Side effects may have occurred.")
}

/// Attachment retention is available only for a trusted transport owner.
/// The journal's replica count also applies to the original reply's storage.
pub(crate) struct ReplyAttachments<'a> {
    attachments: SessionAttachments<'a>,
}

impl<'a> ReplyAttachments<'a> {
    pub(crate) fn new(request: &ToolRequest, js: &'a jetstream::Context, replicas: usize) -> Self {
        // Never infer ownership from model-authored arguments or the opaque
        // parent_session_id. Only the transport's validated local identity works.
        Self {
            attachments: SessionAttachments::for_owner(attachment_owner(request), js, replicas),
        }
    }

    async fn retain(&self, original_bytes: &[u8]) -> Result<Option<String>> {
        self.attachments.retain(original_bytes).await
    }
}

fn attachment_owner(request: &ToolRequest) -> Option<SessionRef> {
    SessionRef::new(
        request
            .parent_agent
            .clone()
            .filter(|agent| !agent.is_empty()),
        request.parent_local_session_id.clone()?,
    )
    .ok()
}

fn encoded_len(reply: &ToolReply) -> Result<usize> {
    Ok(serde_json::to_vec(reply)?.len())
}

/// async-nats 0.50 preserves these typed causes through KV UpdateError. Do not
/// classify text: a tool, auth failure or outage may use the same words.
pub fn is_size_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<jetstream::Error>()
            .is_some_and(|e| e.kind() == jetstream::ErrorCode::STREAM_MESSAGE_EXCEEDS_MAXIMUM)
            || cause
                .downcast_ref::<jetstream::context::PublishError>()
                .is_some_and(|e| {
                    e.kind() == jetstream::context::PublishErrorKind::MaxPayloadExceeded
                })
            || cause
                .downcast_ref::<async_nats::PublishError>()
                .is_some_and(|e| {
                    e.kind() == async_nats::client::PublishErrorKind::MaxPayloadExceeded
                })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_client_and_jetstream_sizes_survive_wrappers() {
        use async_nats::jetstream::{context, kv};
        let client =
            async_nats::PublishError::new(async_nats::client::PublishErrorKind::MaxPayloadExceeded);
        assert!(is_size_error(
            &anyhow::Error::from(client).context("publish tool reply")
        ));
        let publish = context::PublishError::new(context::PublishErrorKind::MaxPayloadExceeded);
        assert!(is_size_error(
            &anyhow::Error::from(kv::UpdateError::from(publish)).context("complete")
        ));
        for kind in [
            context::PublishErrorKind::TimedOut,
            context::PublishErrorKind::Other,
            context::PublishErrorKind::WrongLastSequence,
        ] {
            let error = context::PublishError::new(kind);
            assert!(!is_size_error(&anyhow::Error::from(kv::UpdateError::from(
                error
            ))));
        }
    }

    #[test]
    fn size_words_are_not_a_size_error() {
        for message in [
            "message size exceeds maximum allowed",
            "max payload size exceeded",
            "permission denied (10054)",
            "connection closed",
        ] {
            assert!(!is_size_error(&anyhow::anyhow!(message)));
        }
    }
}
