//! Delivery-only image projection for oversized native tool replies.
//!
//! A successful MCP-shaped result can carry inline base64 images that exceed
//! the NATS completion budget (broker `max_payload`, journal stream
//! `max_message_size`). Instead of failing the call, eligible image blocks are
//! stored as session-owned `cid:media:` attachments and replaced in place with
//! `resource_link` blocks, which the engine hydrates back into images. This
//! module only plans the projection; the caller checks it against the actual
//! budget and performs the uploads, so nothing is stored for a reply whose
//! projection would not fit anyway.

use base64::{engine::general_purpose::STANDARD, Engine};
use harnx_core::{
    attachments::cid_for_data_url_with_session,
    cid_url::{CidUrl, SessionRef},
};
use harnx_toolset::ToolReply;
use serde_json::{json, Value};

/// One image block's upload identity: content CID, decoded bytes and MIME.
pub(crate) struct ImageUpload {
    /// Cache key for uploads acknowledged by the store.
    pub(crate) id: String,
    pub(crate) url: CidUrl,
    pub(crate) bytes: Vec<u8>,
    pub(crate) mime_type: String,
}

/// A projected reply plus the uploads it depends on.
pub(crate) struct ImageOffload {
    pub(crate) reply: ToolReply,
    pub(crate) uploads: Vec<ImageUpload>,
}

/// One block's planned offload: where its link goes, the link itself, and
/// the blob the link points at.
struct Replacement {
    index: usize,
    link: Value,
    upload: ImageUpload,
}

/// Plan replacing eligible image blocks with `resource_link` blocks.
///
/// Returns `None` when the reply cannot be projected: no trusted owner, an
/// errored or malformed result, `isError: true`, or no eligible image block.
/// Whether the projected reply fits the budget is decided by the caller, so
/// this stays a pure function of the candidate.
pub(crate) fn plan_image_offload(
    candidate: &ToolReply,
    owner: Option<&SessionRef>,
) -> Option<ImageOffload> {
    let owner = owner?;
    let content = successful_content_blocks(candidate)?;
    let replacements = content
        .iter()
        .enumerate()
        .filter_map(|(index, block)| image_replacement(owner, index, block))
        .collect::<Vec<_>>();
    assemble_projection(candidate, replacements)
}

/// The top-level `content` blocks of a successful, well-shaped MCP result.
/// `isError: true` results and malformed results never project.
fn successful_content_blocks(candidate: &ToolReply) -> Option<&Vec<Value>> {
    let result = candidate.result.as_ref().ok()?;
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    result.get("content")?.as_array()
}

/// One block's link and upload, or `None` for anything ineligible.
fn image_replacement(owner: &SessionRef, index: usize, block: &Value) -> Option<Replacement> {
    let image = EligibleImage::parse(block)?;
    let data_url = format!("data:{};base64,{}", image.mime_type, image.data);
    // A canonical media CID cannot fail to parse; treating the impossible
    // case as "no projection" keeps the generic retained-error path.
    let cid = CidUrl::parse(&cid_for_data_url_with_session(owner, &data_url)).ok()?;
    let link = link_block(&cid, image.mime_type, image.bytes.len(), block);
    shrinks_the_reply(&link, block).then_some(Replacement {
        index,
        link,
        upload: ImageUpload {
            id: cid.to_string(),
            url: cid,
            bytes: image.bytes,
            mime_type: image.mime_type.to_string(),
        },
    })
}

/// The link must be strictly smaller than the inline block it replaces;
/// otherwise offloading the block would not shrink the reply at all.
fn shrinks_the_reply(link: &Value, block: &Value) -> bool {
    match (serde_json::to_vec(link), serde_json::to_vec(block)) {
        (Ok(link), Ok(block)) => link.len() < block.len(),
        // Unserializable JSON cannot be measured; never offload it.
        _ => false,
    }
}

/// Apply the planned replacements in place, keeping one upload per distinct
/// CID: duplicate blocks share one blob.
fn assemble_projection(
    candidate: &ToolReply,
    replacements: Vec<Replacement>,
) -> Option<ImageOffload> {
    if replacements.is_empty() {
        return None;
    }
    let mut uploads: Vec<ImageUpload> = Vec::new();
    let mut links = Vec::new();
    for replacement in replacements {
        if !uploads
            .iter()
            .any(|upload| upload.id == replacement.upload.id)
        {
            uploads.push(replacement.upload);
        }
        links.push((replacement.index, replacement.link));
    }
    let mut reply = candidate.clone();
    let content = reply
        .result
        .as_mut()
        .ok()?
        .get_mut("content")?
        .as_array_mut()?;
    for (index, link) in links {
        content[index] = link;
    }
    Some(ImageOffload { reply, uploads })
}

/// An image block eligible for offload: `type: "image"`, an `image/*`
/// `mimeType` and `data` that decodes as standard base64. Anything else stays
/// untouched and can never produce a link, because the engine only extracts
/// `resource_link` images with a valid `cid:media:` URI and `image/*` MIME.
struct EligibleImage<'a> {
    data: &'a str,
    mime_type: &'a str,
    bytes: Vec<u8>,
}

impl<'a> EligibleImage<'a> {
    fn parse(block: &'a Value) -> Option<Self> {
        if block.get("type").and_then(Value::as_str) != Some("image") {
            return None;
        }
        let mime_type = block.get("mimeType").and_then(Value::as_str)?;
        if !mime_type.starts_with("image/") {
            return None;
        }
        let data = block.get("data").and_then(Value::as_str)?;
        let bytes = STANDARD.decode(data).ok()?;
        Some(Self {
            data,
            mime_type,
            bytes,
        })
    }
}

/// Build the `resource_link` replacing an inline image block, preserving the
/// original block's annotations, `_meta` and position among its siblings.
fn link_block(url: &CidUrl, mime_type: &str, size: usize, original: &Value) -> Value {
    let name = format!(
        "image.{}",
        mime_type.strip_prefix("image/").unwrap_or("bin")
    );
    let mut link = json!({
        "type": "resource_link",
        "uri": url.to_string(),
        "name": name,
        "mimeType": mime_type,
        "size": size,
    });
    for field in ["annotations", "_meta"] {
        if let Some(value) = original.get(field) {
            link[field] = value.clone();
        }
    }
    link
}
