//! Lazy, read-only attachment bytes with a bounded cache owned by one script run.

use std::{
    collections::HashMap,
    io::{self, ErrorKind},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use async_trait::async_trait;
use bashkit::{DirEntry, FileSystem, FileSystemExt, FileType, FsLimits, FsUsage, Metadata};
use tokio::sync::Mutex;

use crate::FsMount;

/// Storage and authorization stay in the embedder, not the interpreter.
#[async_trait]
pub trait AttachmentFetcher: Send + Sync {
    /// Validate and resolve an explicit media CID. Enforce the caller's access
    /// rules and reject blobs larger than `max_bytes` before allocating its body.
    /// Use `NotFound` for missing blobs. Dropping this future must stop local I/O.
    async fn fetch(&self, cid: &str, max_bytes: u64) -> io::Result<Vec<u8>>;
}

/// Use a fresh instance per invocation. Listings are empty, including after reads:
/// knowledge of a CID grants no directory-enumeration capability.
pub struct AttachmentFs {
    fetcher: Arc<dyn AttachmentFetcher>,
    limits: FsLimits,
    cache: Mutex<HashMap<String, Arc<[u8]>>>,
    bytes: AtomicU64,
    files: AtomicU64,
}

impl AttachmentFs {
    pub fn new(fetcher: Arc<dyn AttachmentFetcher>) -> Self {
        Self::with_limits(fetcher, FsLimits::default())
    }

    pub fn with_limits(fetcher: Arc<dyn AttachmentFetcher>, limits: FsLimits) -> Self {
        Self {
            fetcher,
            limits,
            cache: Mutex::new(HashMap::new()),
            bytes: AtomicU64::new(0),
            files: AtomicU64::new(0),
        }
    }

    pub fn mount(self) -> FsMount {
        FsMount {
            path: PathBuf::from("/attachments"),
            fs: Arc::new(self),
        }
    }

    async fn blob(&self, cid: &str) -> io::Result<Arc<[u8]>> {
        // Serialize misses as well as cache insertion: concurrent readers share
        // one download, and pending downloads cannot overrun the cache budget.
        // Cancellation drops this lock/fetch inline; failed reads aren't cached.
        let mut cache = self.cache.lock().await;
        if let Some(bytes) = cache.get(cid) {
            return Ok(Arc::clone(bytes));
        }
        if cache.len() as u64 >= self.limits.max_file_count {
            return Err(io::Error::other(
                "attachment cache file-count limit exceeded",
            ));
        }
        let used = self.bytes.load(Ordering::Relaxed);
        let remaining = self.limits.max_total_bytes.saturating_sub(used);
        let max_bytes = self.limits.max_file_size.min(remaining);
        let bytes = self.fetcher.fetch(cid, max_bytes).await?;
        if bytes.len() as u64 > max_bytes {
            return Err(io::Error::other(format!(
                "attachment {cid} exceeds the {max_bytes} byte file/cache limit"
            )));
        }
        self.bytes
            .store(used + bytes.len() as u64, Ordering::Relaxed);
        let bytes: Arc<[u8]> = bytes.into();
        cache.insert(cid.to_owned(), Arc::clone(&bytes));
        self.files.store(cache.len() as u64, Ordering::Relaxed);
        Ok(bytes)
    }
}

fn entry<'a>(path: &'a Path, limits: &FsLimits) -> io::Result<Entry<'a>> {
    limits
        .validate_path(path)
        .map_err(|error| io::Error::new(ErrorKind::InvalidInput, error.to_string()))?;
    let path = path
        .to_str()
        .and_then(|path| path.strip_prefix('/'))
        .ok_or_else(invalid_path)?;
    if path.is_empty() {
        return Ok(Entry::Directory);
    }
    let components: Vec<_> = path.split('/').collect();
    if components
        .iter()
        .any(|part| part.is_empty() || matches!(*part, "." | "..") || part.contains('\\'))
        || !components[0].starts_with("cid:media:")
        || components[0] == "cid:media:"
    {
        return Err(invalid_path());
    }
    match components.len() {
        1 | 2 => Ok(Entry::Directory),
        3 => Ok(Entry::Blob(path)),
        _ => Err(invalid_path()),
    }
}

enum Entry<'a> {
    Directory,
    Blob(&'a str),
}

fn invalid_path() -> io::Error {
    io::Error::new(
        ErrorKind::InvalidInput,
        "expected /attachments/cid:media:<encoded-agent>/<session>/<sha256>",
    )
}

/// Keep the CID's own agent encoding (for example `%2F`) unchanged. No second
/// URL-encoding layer is used; colons and slashes are valid BashKit VFS paths.
pub fn attachment_path(cid: &str) -> io::Result<PathBuf> {
    let path = PathBuf::from(format!("/attachments/{cid}"));
    let relative = PathBuf::from(format!("/{cid}"));
    match entry(&relative, &FsLimits::default())? {
        Entry::Blob(_) => Ok(path),
        Entry::Directory => Err(invalid_path()),
    }
}

fn readonly_error() -> bashkit::Error {
    io::Error::new(
        ErrorKind::PermissionDenied,
        "EROFS: attachment filesystem is read-only",
    )
    .into()
}

fn metadata(file_type: FileType, size: u64) -> Metadata {
    Metadata {
        file_type,
        size,
        mode: if file_type.is_dir() { 0o555 } else { 0o444 },
        modified: std::time::SystemTime::UNIX_EPOCH,
        created: std::time::SystemTime::UNIX_EPOCH,
    }
}

#[async_trait]
impl FileSystemExt for AttachmentFs {
    fn usage(&self) -> FsUsage {
        FsUsage::new(
            self.bytes.load(Ordering::Relaxed),
            self.files.load(Ordering::Relaxed),
            1,
        )
    }

    fn limits(&self) -> FsLimits {
        self.limits.clone()
    }

    fn backend_kind(&self) -> &'static str {
        "attachments"
    }

    async fn mkfifo(&self, _path: &Path, _mode: u32) -> bashkit::Result<()> {
        Err(readonly_error())
    }
}

#[async_trait]
impl FileSystem for AttachmentFs {
    async fn read_file(&self, path: &Path) -> bashkit::Result<Vec<u8>> {
        match entry(path, &self.limits)? {
            Entry::Directory => Err(io::Error::other("is a directory").into()),
            Entry::Blob(cid) => Ok(self.blob(cid).await?.to_vec()),
        }
    }

    async fn stat(&self, path: &Path) -> bashkit::Result<Metadata> {
        match entry(path, &self.limits)? {
            Entry::Directory => Ok(metadata(FileType::Directory, 0)),
            Entry::Blob(cid) => Ok(metadata(FileType::File, self.blob(cid).await?.len() as u64)),
        }
    }

    async fn read_dir(&self, path: &Path) -> bashkit::Result<Vec<DirEntry>> {
        match entry(path, &self.limits)? {
            Entry::Directory => Ok(Vec::new()),
            Entry::Blob(_) => Err(io::Error::other("not a directory").into()),
        }
    }

    async fn exists(&self, path: &Path) -> bashkit::Result<bool> {
        match self.stat(path).await {
            Ok(_) => Ok(true),
            Err(bashkit::Error::Io(error)) if error.kind() == ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    async fn read_link(&self, _path: &Path) -> bashkit::Result<PathBuf> {
        Err(io::Error::new(ErrorKind::InvalidInput, "attachment is not a symlink").into())
    }

    async fn write_file(&self, _path: &Path, _content: &[u8]) -> bashkit::Result<()> {
        Err(readonly_error())
    }

    async fn append_file(&self, _path: &Path, _content: &[u8]) -> bashkit::Result<()> {
        Err(readonly_error())
    }

    async fn mkdir(&self, _path: &Path, _recursive: bool) -> bashkit::Result<()> {
        Err(readonly_error())
    }

    async fn remove(&self, _path: &Path, _recursive: bool) -> bashkit::Result<()> {
        Err(readonly_error())
    }

    async fn rename(&self, _from: &Path, _to: &Path) -> bashkit::Result<()> {
        Err(readonly_error())
    }

    async fn copy(&self, _from: &Path, _to: &Path) -> bashkit::Result<()> {
        Err(readonly_error())
    }

    async fn symlink(&self, _target: &Path, _link: &Path) -> bashkit::Result<()> {
        Err(readonly_error())
    }

    async fn chmod(&self, _path: &Path, _mode: u32) -> bashkit::Result<()> {
        Err(readonly_error())
    }

    async fn set_modified_time(
        &self,
        _path: &Path,
        _time: std::time::SystemTime,
    ) -> bashkit::Result<()> {
        Err(readonly_error())
    }
}

#[cfg(test)]
mod tests;
