//! Helpers for annotating rmcp content blocks.
//!
//! rmcp moved audience/priority annotations off the content block and onto each
//! concrete content type (`TextContent`, `ImageContent`, ...) as an embedded
//! `annotations` field. This extension trait restores the ergonomic
//! `content.with_audience(..)` builder so call sites can keep expressing intent
//! ("this text is for the assistant, that summary is for the user") in one line.

use rmcp::model::{Annotations, ContentBlock, Role};

/// Attach an audience annotation to a [`ContentBlock`].
pub trait WithAudience {
    /// Return the content block with its audience annotation set.
    fn with_audience(self, audience: Vec<Role>) -> Self;
}

impl WithAudience for ContentBlock {
    fn with_audience(self, audience: Vec<Role>) -> Self {
        let annotations = Annotations::default().with_audience(audience);
        match self {
            ContentBlock::Text(c) => ContentBlock::Text(c.with_annotations(annotations)),
            ContentBlock::Image(c) => ContentBlock::Image(c.with_annotations(annotations)),
            ContentBlock::Audio(c) => ContentBlock::Audio(c.with_annotations(annotations)),
            ContentBlock::Resource(c) => ContentBlock::Resource(c.with_annotations(annotations)),
            other => other,
        }
    }
}

/// Read the audience annotation from a [`ContentBlock`], if any.
pub fn audience(block: &ContentBlock) -> Option<&Vec<Role>> {
    let annotations = match block {
        ContentBlock::Text(c) => c.annotations.as_ref(),
        ContentBlock::Image(c) => c.annotations.as_ref(),
        ContentBlock::Audio(c) => c.annotations.as_ref(),
        ContentBlock::Resource(c) => c.annotations.as_ref(),
        _ => None,
    };
    annotations.and_then(|a| a.audience.as_ref())
}
