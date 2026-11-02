//! Size limits shared by media reads and plan resolution.

use std::fmt;

use anyhow::Result;

/// A blob exceeded the caller's byte limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobTooLarge {
    /// Metadata size, rendered size, or bytes observed before stopping a read.
    /// For a stopped read, this is a lower bound on the full blob size.
    pub size_bytes: usize,
    /// Maximum number of bytes allowed by the caller.
    pub max_bytes: usize,
}

impl fmt::Display for BlobTooLarge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "blob too large: {} bytes exceeds limit of {} bytes",
            self.size_bytes, self.max_bytes
        )
    }
}

impl std::error::Error for BlobTooLarge {}

pub(crate) fn check_size(size_bytes: usize, max_bytes: Option<usize>) -> Result<()> {
    if let Some(max_bytes) = max_bytes {
        if size_bytes > max_bytes {
            return Err(BlobTooLarge {
                size_bytes,
                max_bytes,
            }
            .into());
        }
    }
    Ok(())
}
