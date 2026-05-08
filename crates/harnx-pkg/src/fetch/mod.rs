use anyhow::Result;
use async_trait::async_trait;

pub mod git;
pub mod oci;

/// Files fetched from a package source, extracted to a temp directory.
pub struct FetchedPackage {
    /// Temporary directory holding the fetched files (root of the package).
    pub dir: tempfile::TempDir,
    /// Resolved commit SHA or OCI digest.
    pub resolved_id: String,
    /// Tag that was fetched.
    pub tag: String,
}

#[async_trait]
pub trait PackageFetcher: Send + Sync {
    /// Fetch the package at the given tag into a temp dir.
    async fn fetch(&self, url: &str, tag: &str, subpath: Option<&str>) -> Result<FetchedPackage>;
    /// List all semver-conforming tags available at the source URL.
    async fn list_tags(&self, url: &str) -> Result<Vec<semver::Version>>;
}
