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

    /// Blob size in bytes without downloading its body. Return `None` for
    /// missing blobs; reads also return `NotFound` for `None`. Must stay cheap
    /// and never consume cache budget: used by metadata probes and cold reads.
    async fn size(&self, cid: &str) -> io::Result<Option<u64>>;
}

/// Use a fresh instance per invocation. Listings are empty, including after reads:
/// knowledge of a CID grants no directory-enumeration capability.
pub struct AttachmentFs {
    fetcher: Arc<dyn AttachmentFetcher>,
    limits: FsLimits,
    cache: Mutex<HashMap<String, Arc<[u8]>>>,
    /// One in-flight download lock per CID, so different CIDs download
    /// concurrently and readers of the same CID share a single fetch. The map
    /// only holds locks that are in flight or have a queued reader: `Flight`
    /// removes its entry once no one else holds it, so failed or fabricated
    /// CIDs don't accumulate.
    downloads: std::sync::Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// Cache bytes already committed plus budgets claimed by in-flight
    /// downloads. Claims and releases are single atomic steps, so together
    /// they can never exceed `max_total_bytes`. Tracked separately from
    /// `bytes` only so `usage()` can report committed bytes.
    budget: AtomicU64,
    /// Bytes actually cached, for `usage()`.
    bytes: AtomicU64,
    files: AtomicU64,
    /// Committed files plus slots owned by in-flight downloads.
    slots: AtomicU64,
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
            downloads: std::sync::Mutex::new(HashMap::new()),
            budget: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            files: AtomicU64::new(0),
            slots: AtomicU64::new(0),
        }
    }

    pub fn mount(self) -> FsMount {
        FsMount {
            path: PathBuf::from("/attachments"),
            fs: Arc::new(self),
        }
    }

    async fn cached(&self, cid: &str) -> Option<Arc<[u8]>> {
        self.cache.lock().await.get(cid).cloned()
    }

    /// Atomically claim the metadata size, capped by the file limit and remaining
    /// cache budget. The claim is released on failure and converted to the real
    /// byte count on success (`Reservation::settle`).
    fn reserve(&self, size: u64) -> Option<u64> {
        let mut current = self.budget.load(Ordering::Relaxed);
        loop {
            let available = self.limits.max_total_bytes.saturating_sub(current);
            let amount = size.min(self.limits.max_file_size).min(available);
            if available == 0 {
                return None;
            }
            match self.budget.compare_exchange_weak(
                current,
                current + amount,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(amount),
                Err(observed) => current = observed,
            }
        }
    }

    async fn blob(&self, cid: &str) -> io::Result<Arc<[u8]>> {
        if let Some(bytes) = self.cached(cid).await {
            return Ok(bytes);
        }
        // A cache miss takes a per-CID download lock, not the cache lock: cache
        // hits, stats and other CIDs never queue behind a download. Queued
        // same-CID readers share one fetch; the lock leaves the map as soon as
        // no one else holds it (Flight's Drop). Dropping this future (backend
        // cancellation) releases the lock, letting a queued reader retry.
        let _flight = Flight::acquire(self, cid).await;
        if let Some(bytes) = self.cached(cid).await {
            return Ok(bytes); // Another reader already downloaded this CID.
        }
        let size = self.fetcher.size(cid).await?.ok_or_else(|| missing(cid))?;
        let mut slot = FileSlot::reserve(self)?;
        // Reserve metadata-sized budget so small concurrent downloads don't
        // each claim a full file allowance. Fetch is bounded by the claim: stale
        // metadata cannot overcommit the cache, even if the fetcher ignores that
        // bound (checked below).
        let reserve = self.reserve(size).ok_or_else(|| {
            io::Error::other(format!(
                "attachment cache is full ({} byte limit)",
                self.limits.max_total_bytes
            ))
        })?;
        let mut reservation = Reservation {
            fs: self,
            amount: Some(reserve),
        };
        let bytes = self.fetcher.fetch(cid, reserve).await?;
        if bytes.len() as u64 > reserve {
            return Err(io::Error::other(format!(
                "attachment {cid} exceeds reserved size of {reserve} bytes"
            )));
        }
        let mut cache = self.cache.lock().await;
        self.bytes.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        let bytes: Arc<[u8]> = bytes.into();
        cache.insert(cid.to_owned(), Arc::clone(&bytes));
        drop(cache);
        self.files.fetch_add(1, Ordering::Relaxed);
        reservation.settle(bytes.len() as u64);
        slot.committed = true;
        Ok(bytes)
    }
}

/// Claim before byte reservation and I/O. A failed byte claim or cancelled
/// download returns its file slot without affecting committed usage.
struct FileSlot<'a> {
    fs: &'a AttachmentFs,
    committed: bool,
}

impl FileSlot<'_> {
    fn reserve(fs: &AttachmentFs) -> io::Result<FileSlot<'_>> {
        fs.slots
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                (current < fs.limits.max_file_count).then(|| current + 1)
            })
            .map_err(|_| io::Error::other("attachment cache file-count limit exceeded"))?;
        Ok(FileSlot {
            fs,
            committed: false,
        })
    }
}

impl Drop for FileSlot<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.fs.slots.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// RAII claim on the cache budget. Dropping releases the full reservation;
/// `settle` replaces it with the committed byte count.
struct Reservation<'a> {
    fs: &'a AttachmentFs,
    amount: Option<u64>,
}

impl Reservation<'_> {
    /// Convert the claim to the committed byte count. One atomic step
    /// (`amount - actual`, with `actual <= amount` guaranteed by the
    /// post-download size check): releasing the claim first and adding the
    /// real size after would transiently under-count the budget and let a
    /// concurrent `reserve` over-admit.
    fn settle(&mut self, actual: u64) {
        let amount = self.amount.take().expect("reservation already settled");
        if amount > actual {
            self.fs.budget.fetch_sub(amount - actual, Ordering::Relaxed);
        }
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        if let Some(amount) = self.amount.take() {
            self.fs.budget.fetch_sub(amount, Ordering::Relaxed);
        }
    }
}

/// Per-CID download lock, removed from the map when its last holder is done.
/// Dropping (including cancellation) releases the lock and, when no other
/// reader holds a clone, deletes the map entry — the map tracks in-flight
/// downloads and their waiters, not every CID ever probed.
struct Flight<'a> {
    fs: &'a AttachmentFs,
    cid: &'a str,
    lock: Option<Arc<Mutex<()>>>,
    guard: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl Flight<'_> {
    async fn acquire<'a>(fs: &'a AttachmentFs, cid: &'a str) -> Flight<'a> {
        let lock = {
            let mut downloads = fs
                .downloads
                .lock()
                .expect("attachment download map poisoned");
            Arc::clone(downloads.entry(cid.to_owned()).or_default())
        };
        // Own cleanup before waiting too: a cancelled last waiter otherwise
        // leaves the map entry behind after the previous holder has gone.
        let mut flight = Flight {
            fs,
            cid,
            lock: Some(Arc::clone(&lock)),
            guard: None,
        };
        flight.guard = Some(lock.lock_owned().await);
        flight
    }
}

impl Drop for Flight<'_> {
    fn drop(&mut self) {
        // Release the lock first: queued readers then hold their own clones,
        // which keeps the entry (and single-flight) alive for their retry.
        self.guard.take();
        let Some(lock) = self.lock.take() else {
            return;
        };
        let mut downloads = self
            .fs
            .downloads
            .lock()
            .expect("attachment download map poisoned");
        // With only the map's entry and this clone left, no reader is queued
        // or downloading this CID: remove it. The map lock is held throughout,
        // so a concurrent acquire cannot clone between the count and remove.
        let idle = downloads
            .get(self.cid)
            .is_some_and(|entry| Arc::ptr_eq(entry, &lock) && Arc::strong_count(entry) == 2);
        if idle {
            downloads.remove(self.cid);
        }
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
    if components.iter().any(|part| invalid_component(part)) {
        return Err(invalid_path());
    }
    if !valid_media_prefix(components[0]) {
        return Err(invalid_path());
    }
    match components.len() {
        1 | 2 => Ok(Entry::Directory),
        3 => Ok(Entry::Blob(path)),
        _ => Err(invalid_path()),
    }
}

fn invalid_component(part: &str) -> bool {
    part.is_empty() || matches!(part, "." | "..") || part.contains('\\')
}

fn valid_media_prefix(part: &str) -> bool {
    part.starts_with("cid:media:") && part != "cid:media:"
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

// EROFS/EISDIR/ENOTDIR appear in message text only: BashKit maps io kinds
// (PermissionDenied -> EACCES, everything else -> EIO), so rely on the
// message, not the errno, for these conditions.
fn readonly_error() -> bashkit::Error {
    io::Error::new(
        ErrorKind::PermissionDenied,
        "EROFS: attachment filesystem is read-only",
    )
    .into()
}

fn missing(cid: &str) -> io::Error {
    io::Error::new(ErrorKind::NotFound, format!("attachment not found: {cid}"))
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
            // Metadata-only: no download, no cache consumption. Repeated
            // `test -f` probes stay cheap even when the cache is full.
            Entry::Blob(cid) => {
                let size = self.fetcher.size(cid).await?.ok_or_else(|| missing(cid))?;
                Ok(metadata(FileType::File, size))
            }
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
