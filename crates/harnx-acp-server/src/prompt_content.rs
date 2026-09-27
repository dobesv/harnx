//! Prompt content conversion from ACP ContentBlock to harnx Input text.
//!
//! ACP v1 `session/prompt` carries a list of `ContentBlock` variants. This module
//! converts supported block types to plain text for harnx's `Input::prompt` field,
//! while returning explicit errors for unsupported block types instead of silently
//! dropping them.
//!
//! # Supported block types
//!
//! - `TextContent`: passed through verbatim.
//! - `EmbeddedResource` with `TextResourceContents`: delimited text with URI and MIME provenance.
//! - `ResourceLink`: formatted reference with name, URI, and optional metadata.
//!
//! # Unsupported block types (explicit errors)
//!
//! - `ImageContent`: images are not supported as prompt input.
//! - `AudioContent`: audio is not supported as prompt input.
//! - `EmbeddedResource` with `BlobResourceContents`: binary blobs are not supported.
//!
//! All blocks are joined in the order they appear in the request. If any block is
//! unsupported, the entire conversion fails immediately without partial output.

use agent_client_protocol::schema::v1::{
    ContentBlock, EmbeddedResource, EmbeddedResourceResource, ResourceLink, TextContent,
    TextResourceContents,
};
use std::fmt;

/// Error returned when prompt content conversion fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptContentError {
    /// Prompt contains no content blocks or converts to entirely whitespace.
    Empty,
    /// Image content blocks are not supported.
    ImageNotSupported,
    /// Audio content blocks are not supported.
    AudioNotSupported,
    /// Binary blob resources are not supported.
    BlobResourceNotSupported,
    /// An unknown or unsupported content block variant was encountered.
    UnknownBlock {
        /// Description of the block kind for diagnostics.
        kind: String,
    },
}

impl fmt::Display for PromptContentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "prompt content must not be empty"),
            Self::ImageNotSupported => write!(f, "image content blocks are not supported"),
            Self::AudioNotSupported => write!(f, "audio content blocks are not supported"),
            Self::BlobResourceNotSupported => {
                write!(f, "embedded binary blob resources are not supported")
            }
            Self::UnknownBlock { kind } => {
                write!(f, "unsupported content block type: {kind}")
            }
        }
    }
}

impl std::error::Error for PromptContentError {}

/// Parse prompt content from an ACP `PromptRequest`.
///
/// Converts supported `ContentBlock` variants to text in order, returning an error
/// for any unsupported block type. The conversion fails immediately on the first
/// unsupported block; no partial conversion is attempted.
///
/// # Arguments
///
/// * `request` - The ACP prompt request containing a list of content blocks.
///
/// # Returns
///
/// * `Ok(String)` - The converted text, with blocks joined by newlines.
/// * `Err(PromptContentError::Empty)` - If the request has no blocks or all blocks yield whitespace.
/// * `Err(PromptContentError::*)` - If any block is unsupported.
pub fn parse_prompt_content(
    request: &agent_client_protocol::schema::v1::PromptRequest,
) -> Result<String, PromptContentError> {
    if request.prompt.is_empty() {
        return Err(PromptContentError::Empty);
    }

    let converted: Vec<String> = request
        .prompt
        .iter()
        .map(convert_block)
        .collect::<Result<Vec<_>, _>>()?;

    let text = converted.join("\n");

    if text.trim().is_empty() {
        return Err(PromptContentError::Empty);
    }

    Ok(text)
}

/// Convert a single content block to its text representation.
fn convert_block(block: &ContentBlock) -> Result<String, PromptContentError> {
    match block {
        ContentBlock::Text(text_content) => Ok(convert_text_block(text_content)),
        ContentBlock::Resource(embedded) => convert_embedded_resource(embedded),
        ContentBlock::ResourceLink(link) => Ok(convert_resource_link(link)),
        ContentBlock::Image(_) => Err(PromptContentError::ImageNotSupported),
        ContentBlock::Audio(_) => Err(PromptContentError::AudioNotSupported),
        // ContentBlock is #[non_exhaustive], so we need a fallback for unknown variants.
        _ => Err(PromptContentError::UnknownBlock {
            kind: format!("{:?}", block),
        }),
    }
}

/// Convert a `TextContent` block to plain text.
fn convert_text_block(text: &TextContent) -> String {
    text.text.clone()
}

/// Convert an `EmbeddedResource` block to delimited text.
///
/// Text resources include URI and MIME provenance in the output.
/// Binary blob resources return an error.
fn convert_embedded_resource(embedded: &EmbeddedResource) -> Result<String, PromptContentError> {
    match &embedded.resource {
        EmbeddedResourceResource::TextResourceContents(text_resource) => {
            Ok(convert_text_resource(text_resource))
        }
        EmbeddedResourceResource::BlobResourceContents(_) => {
            Err(PromptContentError::BlobResourceNotSupported)
        }
        // Handle unknown variants of EmbeddedResourceResource.
        _ => Err(PromptContentError::UnknownBlock {
            kind: format!("EmbeddedResource({:?})", embedded.resource),
        }),
    }
}

/// Format a text resource with URI and MIME provenance.
fn convert_text_resource(resource: &TextResourceContents) -> String {
    let mime = resource.mime_type.as_deref().unwrap_or("unspecified");
    format!(
        "--- Embedded Resource: {} (MIME: {}) ---\n{}\n--- End Embedded Resource: {} ---",
        resource.uri, mime, resource.text, resource.uri
    )
}

/// Format a resource link as a reference with optional metadata.
fn convert_resource_link(link: &ResourceLink) -> String {
    let mut metadata_parts = vec![format!("uri={}", link.uri)];

    if let Some(ref title) = link.title {
        metadata_parts.push(format!("title={}", title));
    }
    if let Some(ref desc) = link.description {
        metadata_parts.push(format!("description={}", desc));
    }
    if let Some(ref mime) = link.mime_type {
        metadata_parts.push(format!("mime={}", mime));
    }
    if let Some(size) = link.size {
        metadata_parts.push(format!("size={}", size));
    }

    format!(
        "[Resource Link: {} ({})]",
        link.name,
        metadata_parts.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
        AudioContent, BlobResourceContents, ImageContent, PromptRequest, SessionId,
    };

    fn session_id() -> SessionId {
        SessionId::new("test".to_string())
    }

    #[test]
    fn text_only_single_block() {
        let request = PromptRequest::new(
            session_id(),
            vec![ContentBlock::Text(TextContent::new(
                "hello world".to_string(),
            ))],
        );
        assert_eq!(parse_prompt_content(&request).unwrap(), "hello world");
    }

    #[test]
    fn text_only_multiple_blocks() {
        let request = PromptRequest::new(
            session_id(),
            vec![
                ContentBlock::Text(TextContent::new("first".to_string())),
                ContentBlock::Text(TextContent::new("second".to_string())),
            ],
        );
        assert_eq!(parse_prompt_content(&request).unwrap(), "first\nsecond");
    }

    #[test]
    fn text_preserves_whitespace() {
        let request = PromptRequest::new(
            session_id(),
            vec![ContentBlock::Text(TextContent::new(
                "  indented  ".to_string(),
            ))],
        );
        assert_eq!(parse_prompt_content(&request).unwrap(), "  indented  ");
    }

    #[test]
    fn embedded_text_resource_with_mime() {
        let text_resource = TextResourceContents::new("resource content", "file:///test.txt")
            .mime_type("text/plain".to_string());
        let request = PromptRequest::new(
            session_id(),
            vec![ContentBlock::Resource(EmbeddedResource::new(
                EmbeddedResourceResource::TextResourceContents(text_resource),
            ))],
        );
        let result = parse_prompt_content(&request).unwrap();
        assert!(result.contains("--- Embedded Resource: file:///test.txt (MIME: text/plain) ---"));
        assert!(result.contains("resource content"));
        assert!(result.contains("--- End Embedded Resource: file:///test.txt ---"));
    }

    #[test]
    fn embedded_text_resource_without_mime() {
        let text_resource = TextResourceContents::new("content", "file:///test.txt");
        let request = PromptRequest::new(
            session_id(),
            vec![ContentBlock::Resource(EmbeddedResource::new(
                EmbeddedResourceResource::TextResourceContents(text_resource),
            ))],
        );
        let result = parse_prompt_content(&request).unwrap();
        assert!(result.contains("(MIME: unspecified)"));
    }

    #[test]
    fn resource_link_minimal() {
        let link = ResourceLink::new(
            "my_resource".to_string(),
            "file:///path/to/resource".to_string(),
        );
        let request = PromptRequest::new(session_id(), vec![ContentBlock::ResourceLink(link)]);
        let result = parse_prompt_content(&request).unwrap();
        assert_eq!(
            result,
            "[Resource Link: my_resource (uri=file:///path/to/resource)]"
        );
    }

    #[test]
    fn resource_link_full_metadata() {
        let link = ResourceLink::new("doc".to_string(), "file:///doc.md".to_string())
            .title("Documentation".to_string())
            .description("User guide".to_string())
            .mime_type("text/markdown".to_string())
            .size(1234);
        let request = PromptRequest::new(session_id(), vec![ContentBlock::ResourceLink(link)]);
        let result = parse_prompt_content(&request).unwrap();
        assert!(result.contains("[Resource Link: doc ("));
        assert!(result.contains("uri=file:///doc.md"));
        assert!(result.contains("title=Documentation"));
        assert!(result.contains("description=User guide"));
        assert!(result.contains("mime=text/markdown"));
        assert!(result.contains("size=1234"));
    }

    #[test]
    fn mixed_blocks_in_order() {
        let request = PromptRequest::new(
            session_id(),
            vec![
                ContentBlock::Text(TextContent::new("intro".to_string())),
                ContentBlock::ResourceLink(ResourceLink::new(
                    "link".to_string(),
                    "uri:x".to_string(),
                )),
                ContentBlock::Text(TextContent::new("outro".to_string())),
            ],
        );
        let result = parse_prompt_content(&request).unwrap();
        let lines: Vec<&str> = result.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("intro"));
        assert!(lines[1].starts_with("[Resource Link:"));
        assert_eq!(lines[2], "outro");
    }

    #[test]
    fn empty_prompt_rejected() {
        let request = PromptRequest::new(session_id(), vec![]);
        let err = parse_prompt_content(&request).unwrap_err();
        assert_eq!(err, PromptContentError::Empty);
    }

    #[test]
    fn whitespace_only_rejected() {
        let request = PromptRequest::new(
            session_id(),
            vec![ContentBlock::Text(TextContent::new("   ".to_string()))],
        );
        let err = parse_prompt_content(&request).unwrap_err();
        assert_eq!(err, PromptContentError::Empty);
    }

    #[test]
    fn image_rejected() {
        let request = PromptRequest::new(
            session_id(),
            vec![ContentBlock::Image(ImageContent::new(
                "base64".to_string(),
                "image/png".to_string(),
            ))],
        );
        let err = parse_prompt_content(&request).unwrap_err();
        assert_eq!(err, PromptContentError::ImageNotSupported);
        assert!(err.to_string().contains("image"));
    }

    #[test]
    fn audio_rejected() {
        let request = PromptRequest::new(
            session_id(),
            vec![ContentBlock::Audio(AudioContent::new(
                "base64".to_string(),
                "audio/mp3".to_string(),
            ))],
        );
        let err = parse_prompt_content(&request).unwrap_err();
        assert_eq!(err, PromptContentError::AudioNotSupported);
        assert!(err.to_string().contains("audio"));
    }

    #[test]
    fn binary_blob_resource_rejected() {
        let blob =
            BlobResourceContents::new("base64blob".to_string(), "file:///binary.bin".to_string());
        let request = PromptRequest::new(
            session_id(),
            vec![ContentBlock::Resource(EmbeddedResource::new(
                EmbeddedResourceResource::BlobResourceContents(blob),
            ))],
        );
        let err = parse_prompt_content(&request).unwrap_err();
        assert_eq!(err, PromptContentError::BlobResourceNotSupported);
        assert!(err.to_string().contains("binary"));
    }

    #[test]
    fn mixed_valid_and_invalid_rejected() {
        let request = PromptRequest::new(
            session_id(),
            vec![
                ContentBlock::Text(TextContent::new("valid text".to_string())),
                ContentBlock::Image(ImageContent::new(
                    "data".to_string(),
                    "image/png".to_string(),
                )),
            ],
        );
        let err = parse_prompt_content(&request).unwrap_err();
        // Should fail on the first unsupported block (image)
        assert_eq!(err, PromptContentError::ImageNotSupported);
    }

    #[test]
    fn embedded_resource_with_text_and_resource_link() {
        let text_resource = TextResourceContents::new("embedded", "uri:embedded");
        let request = PromptRequest::new(
            session_id(),
            vec![
                ContentBlock::Text(TextContent::new("text".to_string())),
                ContentBlock::Resource(EmbeddedResource::new(
                    EmbeddedResourceResource::TextResourceContents(text_resource),
                )),
                ContentBlock::ResourceLink(ResourceLink::new(
                    "link".to_string(),
                    "uri:link".to_string(),
                )),
            ],
        );
        let result = parse_prompt_content(&request).unwrap();
        assert!(result.starts_with("text\n"));
        assert!(result.contains("--- Embedded Resource: uri:embedded"));
        assert!(result.contains("[Resource Link: link"));
    }
}
