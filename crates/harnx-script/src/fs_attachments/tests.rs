use std::{sync::atomic::AtomicUsize, time::Duration};

use super::*;
use crate::{BashKitBackend, CancellationToken, ScriptBackend, ScriptLimits, ScriptRun};

mod budget;

const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xb5, 0x1c, 0x0c,
    0x02, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0xfc, 0xff, 0x1f, 0x00,
    0x03, 0x03, 0x02, 0x00, 0xef, 0xa2, 0xa7, 0x5b, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44,
    0xae, 0x42, 0x60, 0x82,
];
const PNG_BASE64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+ip1sAAAAASUVORK5CYII=";

fn cid(letter: char) -> String {
    format!(
        "cid:media:pantheon%2Fatlas/attach1/{}",
        letter.to_string().repeat(64)
    )
}

struct FakeFetcher {
    calls: AtomicUsize,
    blobs: HashMap<String, Vec<u8>>,
}

impl FakeFetcher {
    fn png() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            blobs: HashMap::from([(cid('a'), PNG.to_vec())]),
        })
    }
}

#[async_trait]
impl AttachmentFetcher for FakeFetcher {
    async fn fetch(&self, cid: &str, _max_bytes: u64) -> io::Result<Vec<u8>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        // Deliberately ignore the byte limit to prove the filesystem checks too.
        self.blobs.get(cid).cloned().ok_or_else(|| {
            io::Error::new(ErrorKind::NotFound, format!("attachment not found: {cid}"))
        })
    }

    async fn size(&self, cid: &str) -> io::Result<Option<u64>> {
        Ok(self.blobs.get(cid).map(|bytes| bytes.len() as u64))
    }
}

/// Sizes without downloading, mirroring what a metadata-only broker stat
/// does in the embedder.
struct SizingFetcher {
    fetches: AtomicUsize,
    sizes: AtomicUsize,
    blobs: HashMap<String, Vec<u8>>,
}

#[async_trait]
impl AttachmentFetcher for SizingFetcher {
    async fn fetch(&self, cid: &str, _max_bytes: u64) -> io::Result<Vec<u8>> {
        self.fetches.fetch_add(1, Ordering::SeqCst);
        Ok(self.blobs.get(cid).cloned().unwrap_or_default())
    }

    async fn size(&self, cid: &str) -> io::Result<Option<u64>> {
        self.sizes.fetch_add(1, Ordering::SeqCst);
        Ok(self.blobs.get(cid).map(|bytes| bytes.len() as u64))
    }
}

async fn script(script: &str, fs: AttachmentFs) -> ScriptRun {
    BashKitBackend
        .run(crate::ScriptRequest {
            script,
            limits: ScriptLimits::default(),
            commands: vec![],
            fs_mounts: vec![fs.mount()],
            cancel: CancellationToken::new(),
        })
        .await
}

#[track_caller]
fn assert_usage(fs: &AttachmentFs, bytes: u64, files: u64) {
    assert_eq!(fs.usage().total_bytes, bytes);
    assert_eq!(fs.usage().file_count, files);
}

#[track_caller]
fn assert_settled_budget(fs: &AttachmentFs) {
    assert_eq!(fs.budget.load(Ordering::Relaxed), fs.usage().total_bytes);
    assert_eq!(fs.slots.load(Ordering::Relaxed), fs.usage().file_count);
}

#[track_caller]
fn assert_no_download_locks(fs: &AttachmentFs, message: &str) {
    assert!(fs.downloads.lock().unwrap().is_empty(), "{message}");
}

#[track_caller]
fn assert_cache_full(error: &str, limit: u64) {
    assert!(
        error.contains(&format!("attachment cache is full ({limit} byte limit)")),
        "{error}"
    );
    assert!(
        !error.contains("reserved size of 0 bytes"),
        "full-cache error must name the cache limit: {error}"
    );
}

async fn assert_file_metadata(fs: &AttachmentFs, path: &Path, size: u64) {
    let metadata = fs.stat(path).await.unwrap();
    assert_eq!(metadata.size, size);
    assert!(metadata.file_type.is_file());
    assert!(fs.exists(path).await.unwrap());
}

#[track_caller]
fn assert_download_count(fetcher: &SizingFetcher, expected: usize) {
    assert_eq!(
        fetcher.fetches.load(Ordering::SeqCst),
        expected,
        "unexpected downloads"
    );
}

#[track_caller]
fn assert_script_stdout(result: &ScriptRun, stdout: &str) {
    assert_eq!(result.exit_code, 0, "{}", result.stderr);
    assert_eq!(result.stdout, stdout);
}

async fn download_counts(
    readers: Vec<tokio::task::JoinHandle<bashkit::Result<Vec<u8>>>>,
    size: usize,
    cache_limit: u64,
) -> (usize, usize) {
    let mut succeeded = 0;
    let mut failed = 0;
    for reader in readers {
        let result = tokio::time::timeout(Duration::from_secs(2), reader)
            .await
            .unwrap()
            .unwrap();
        match result {
            Ok(bytes) => {
                assert_eq!(bytes.len(), size);
                succeeded += 1;
            }
            Err(error) => {
                assert_cache_full(&error.to_string(), cache_limit);
                failed += 1;
            }
        }
    }
    (succeeded, failed)
}

#[tokio::test]
async fn full_cid_binary_png_pipeline_and_copy_are_lossless() {
    let fetcher = FakeFetcher::png();
    let path = attachment_path(&cid('a')).unwrap().display().to_string();
    let result = script(&format!(
        "ls /attachments; base64 -w0 '{path}' | wc -c; cp '{path}' /tmp/image.png; base64 -w0 /tmp/image.png; echo; ln -s '{path}' /tmp/link; base64 -w0 /tmp/link; echo; ls /attachments"
    ), AttachmentFs::new(fetcher.clone())).await;
    assert_eq!(result.exit_code, 0, "{}", result.stderr);
    assert_eq!(
        result.stdout,
        format!("{}\n{PNG_BASE64}\n{PNG_BASE64}\n", PNG_BASE64.len())
    );
    assert_eq!(fetcher.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn lazy_cache_is_per_mount_and_directories_never_enumerate() {
    let fetcher = FakeFetcher::png();
    let fs = AttachmentFs::new(fetcher.clone());
    for path in [
        "/",
        "/cid:media:pantheon%2Fatlas",
        "/cid:media:pantheon%2Fatlas/attach1",
    ] {
        assert!(fs.stat(Path::new(path)).await.unwrap().file_type.is_dir());
        assert!(fs.read_dir(Path::new(path)).await.unwrap().is_empty());
    }
    assert_eq!(fetcher.calls.load(Ordering::SeqCst), 0);
    let path = PathBuf::from(format!("/{}", cid('a')));
    assert_eq!(fs.stat(&path).await.unwrap().size, PNG.len() as u64);
    assert_eq!(fs.read_file(&path).await.unwrap(), PNG);
    assert!(fs.exists(&path).await.unwrap());
    assert!(fs.read_dir(Path::new("/")).await.unwrap().is_empty());
    assert_usage(&fs, PNG.len() as u64, 1);
    assert_eq!(fetcher.calls.load(Ordering::SeqCst), 1);
    let fresh = AttachmentFs::new(fetcher.clone());
    assert_eq!(fresh.read_file(&path).await.unwrap(), PNG);
    assert_eq!(fetcher.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn all_mutations_fail_readonly_and_shell_cannot_replace_mount_data() {
    let fetcher = FakeFetcher::png();
    let fs = AttachmentFs::new(fetcher.clone());
    let path = PathBuf::from(format!("/{}", cid('a')));
    let other = Path::new("/new");
    let errors = vec![
        fs.write_file(&path, b"bad").await,
        fs.append_file(&path, b"bad").await,
        fs.mkdir(other, true).await,
        fs.remove(&path, true).await,
        fs.rename(&path, other).await,
        fs.copy(&path, other).await,
        fs.symlink(&path, other).await,
        fs.chmod(&path, 0o777).await,
        fs.mkfifo(other, 0o644).await,
        fs.set_modified_time(&path, std::time::SystemTime::now())
            .await,
    ];
    for result in errors {
        assert!(result.unwrap_err().to_string().contains("EROFS"));
    }
    assert_eq!(fetcher.calls.load(Ordering::SeqCst), 0);
    let mounted = attachment_path(&cid('a')).unwrap().display().to_string();
    for command in [
        format!("echo bad > '{mounted}'"),
        format!("echo bad >> '{mounted}'"),
        "mkdir /attachments/new".to_owned(),
        format!("rm '{mounted}'"),
        format!("mv '{mounted}' /tmp/moved"),
        format!("cp /etc/hostname '{mounted}'"),
        format!("chmod 777 '{mounted}'"),
        format!(
            "ln -s /tmp/anything '{}'",
            attachment_path(&cid('b')).unwrap().display()
        ),
    ] {
        let result = script(&command, AttachmentFs::new(fetcher.clone())).await;
        assert_ne!(result.exit_code, 0, "{command}");
        assert!(
            result.stderr.contains("read-only"),
            "{command}: {}",
            result.stderr
        );
    }
    assert_eq!(fs.read_file(&path).await.unwrap(), PNG);
}

#[tokio::test]
async fn missing_and_malformed_cids_have_clear_errors_without_poisoning_cache() {
    let fetcher = FakeFetcher::png();
    let fs = AttachmentFs::new(fetcher.clone());
    let missing = PathBuf::from(format!("/{}", cid('b')));
    assert!(!fs.exists(&missing).await.unwrap());
    assert!(fs
        .read_file(&missing)
        .await
        .unwrap_err()
        .to_string()
        .contains("attachment not found"));
    assert_eq!(fs.usage().file_count, 0);
    for value in [
        "cid:plan:agent/session/plan",
        "file:///etc/passwd",
        "../tmp/x",
        "cid:media:agent/session/../x",
        "cid:media:agent/session/hash/extra",
        "cid:media:agent/session",
        "cid:media:agent\\other/session/hash",
    ] {
        assert!(attachment_path(value).is_err(), "{value}");
    }
    assert_eq!(
        fs.read_file(&PathBuf::from(format!("/{}", cid('a'))))
            .await
            .unwrap(),
        PNG
    );
    let result = script(
        &format!("cat '{}'", attachment_path(&cid('b')).unwrap().display()),
        fs,
    )
    .await;
    assert_ne!(result.exit_code, 0);
    assert!(
        result.stderr.contains("attachment not found"),
        "{}",
        result.stderr
    );
}

#[tokio::test]
async fn file_total_and_count_limits_fail_without_truncation_or_cache_growth() {
    let fetcher = Arc::new(FakeFetcher {
        calls: AtomicUsize::new(0),
        blobs: HashMap::from([(cid('a'), vec![0, 255, 1]), (cid('b'), vec![2, 3, 4])]),
    });
    let a = PathBuf::from(format!("/{}", cid('a')));
    let b = PathBuf::from(format!("/{}", cid('b')));
    let fs = AttachmentFs::with_limits(fetcher.clone(), FsLimits::default().max_file_size(2));
    assert!(fs
        .read_file(&a)
        .await
        .unwrap_err()
        .to_string()
        .contains("exceeds reserved size of 2 bytes"));
    assert_eq!(fs.usage().total_bytes, 0);
    assert_eq!(fs.usage().file_count, 0);
    let fs = AttachmentFs::with_limits(fetcher.clone(), FsLimits::default().max_total_bytes(5));
    assert_eq!(fs.read_file(&a).await.unwrap(), [0, 255, 1]);
    assert!(fs.read_file(&b).await.is_err());
    assert_eq!(fs.usage().total_bytes, 3);
    assert_eq!(fs.read_file(&a).await.unwrap(), [0, 255, 1]);
    let fs = AttachmentFs::with_limits(fetcher.clone(), FsLimits::default().max_file_count(1));
    fs.read_file(&a).await.unwrap();
    let calls = fetcher.calls.load(Ordering::SeqCst);
    assert!(fs
        .read_file(&b)
        .await
        .unwrap_err()
        .to_string()
        .contains("file-count limit"));
    assert_eq!(fetcher.calls.load(Ordering::SeqCst), calls);
}

struct GatedFetcher {
    started: tokio::sync::mpsc::UnboundedSender<()>,
    release: tokio::sync::Semaphore,
    active: Arc<AtomicUsize>,
}

struct Active(Arc<AtomicUsize>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl AttachmentFetcher for GatedFetcher {
    async fn fetch(&self, _cid: &str, _max_bytes: u64) -> io::Result<Vec<u8>> {
        self.active.fetch_add(1, Ordering::SeqCst);
        let _active = Active(self.active.clone());
        self.started.send(()).unwrap();
        self.release.acquire().await.unwrap().forget();
        Ok(PNG.to_vec())
    }

    async fn size(&self, _cid: &str) -> io::Result<Option<u64>> {
        Ok(Some(PNG.len() as u64))
    }
}

/// Blocks inside `fetch` until released, then returns a fixed-size blob. Lets
/// tests observe exactly which reservations were claimed.
struct SignalFetcher {
    started: tokio::sync::mpsc::UnboundedSender<()>,
    release: tokio::sync::Semaphore,
    size: usize,
}

#[async_trait]
impl AttachmentFetcher for SignalFetcher {
    async fn fetch(&self, _cid: &str, max_bytes: u64) -> io::Result<Vec<u8>> {
        assert!(max_bytes >= self.size as u64);
        self.started.send(()).unwrap();
        self.release.acquire().await.unwrap().forget();
        Ok(vec![7u8; self.size])
    }

    async fn size(&self, _cid: &str) -> io::Result<Option<u64>> {
        Ok(Some(self.size as u64))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_reads_share_one_fetch_and_cancelled_fetch_can_retry() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let fetcher = Arc::new(GatedFetcher {
        started: tx,
        release: tokio::sync::Semaphore::new(0),
        active: Arc::new(AtomicUsize::new(0)),
    });
    let fs = Arc::new(AttachmentFs::new(fetcher.clone()));
    let path = PathBuf::from(format!("/{}", cid('a')));
    let reader = |fs: Arc<AttachmentFs>, path: PathBuf| {
        tokio::spawn(async move { fs.read_file(&path).await })
    };
    let first = reader(fs.clone(), path.clone());
    tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    let second = reader(fs.clone(), path.clone());
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    // The queued reader retries after cancellation, without a leaked lock or entry.
    tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fetcher.active.load(Ordering::SeqCst), 1);
    let third = reader(fs.clone(), path);
    fetcher.release.add_permits(1);
    assert_eq!(second.await.unwrap().unwrap(), PNG);
    assert_eq!(third.await.unwrap().unwrap(), PNG);
    assert!(
        rx.try_recv().is_err(),
        "cache hits must not start another fetch"
    );
    assert_eq!(fetcher.active.load(Ordering::SeqCst), 0);
    assert_eq!(fs.usage().file_count, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_distinct_cid_downloads_cannot_overshoot_the_byte_limit() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let fetcher = Arc::new(SignalFetcher {
        started: tx,
        release: tokio::sync::Semaphore::new(0),
        size: 10,
    });
    // Two 10-byte blobs fit the 20-byte cache. Each download claims a 10-byte
    // reservation before fetching, so only two of the ten distinct CIDs may
    // start a download; the rest must fail at claim time and the cache must
    // never hold more than max_total_bytes.
    let fs = Arc::new(AttachmentFs::with_limits(
        fetcher.clone(),
        FsLimits::default().max_total_bytes(20).max_file_size(10),
    ));
    let readers: Vec<_> = ('a'..='j')
        .map(|letter| {
            let fs = fs.clone();
            let path = PathBuf::from(format!("/{}", cid(letter)));
            tokio::spawn(async move { fs.read_file(&path).await })
        })
        .collect();
    for _ in 0..2 {
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
    }
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert!(
        rx.try_recv().is_err(),
        "downloads beyond the claimed budget must not start"
    );
    fetcher.release.add_permits(2);
    let counts = download_counts(readers, 10, 20).await;
    assert_eq!(counts, (2, 8));
    assert!(fs.usage().total_bytes <= 20, "{}", fs.usage().total_bytes);
    assert_usage(&fs, 20, 2);
    // The claimed budget is fully converted to used bytes; a new read still
    // fails fast because the cache is full.
    let error = fs
        .read_file(&PathBuf::from(format!("/{}", cid('k'))))
        .await
        .unwrap_err()
        .to_string();
    assert_cache_full(&error, 20);
}

#[tokio::test]
async fn settle_returns_the_unclaimed_budget_in_one_step() {
    let fetcher = Arc::new(SizingFetcher {
        fetches: AtomicUsize::new(0),
        sizes: AtomicUsize::new(0),
        blobs: HashMap::from([(cid('a'), vec![0, 255, 1]), (cid('b'), vec![2, 3, 4])]),
    });
    // Metadata-sized claims leave room for both 3-byte blobs, and the budget
    // must equal the committed bytes after each download settles.
    let fs = AttachmentFs::with_limits(
        fetcher.clone(),
        FsLimits::default().max_file_size(10).max_total_bytes(7),
    );
    assert_eq!(
        fs.read_file(&PathBuf::from(format!("/{}", cid('a'))))
            .await
            .unwrap(),
        [0, 255, 1]
    );
    assert_usage(&fs, 3, 1);
    assert_settled_budget(&fs);
    assert_eq!(
        fs.read_file(&PathBuf::from(format!("/{}", cid('b'))))
            .await
            .unwrap(),
        [2, 3, 4]
    );
    assert_settled_budget(&fs);
    assert_usage(&fs, 6, 2);
}

#[tokio::test]
async fn download_locks_leave_the_map_when_no_reader_waits() {
    let fetcher = FakeFetcher::png();
    let fs = AttachmentFs::new(fetcher.clone());
    let path = PathBuf::from(format!("/{}", cid('a')));
    assert_eq!(fs.read_file(&path).await.unwrap(), PNG);
    assert_no_download_locks(&fs, "a completed download must remove its lock entry");
    // Failed reads don't leave entries either.
    assert!(fs
        .read_file(&PathBuf::from(format!("/{}", cid('b'))))
        .await
        .is_err());
    assert_no_download_locks(&fs, "a failed download must remove its lock entry");
    // Reads still work after cleanup.
    assert_eq!(fs.read_file(&path).await.unwrap(), PNG);
    assert_usage(&fs, PNG.len() as u64, 1);
}

#[tokio::test]
async fn backend_cancellation_drops_pending_attachment_download() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let fetcher = Arc::new(GatedFetcher {
        started: tx,
        release: tokio::sync::Semaphore::new(0),
        active: Arc::new(AtomicUsize::new(0)),
    });
    let token = CancellationToken::new();
    let cancel = token.clone();
    let mount = AttachmentFs::new(fetcher.clone()).mount();
    let command = format!("cat '{}'", attachment_path(&cid('a')).unwrap().display());
    let task = tokio::spawn(async move {
        BashKitBackend
            .run(crate::ScriptRequest {
                script: &command,
                limits: ScriptLimits::default(),
                commands: vec![],
                fs_mounts: vec![mount],
                cancel: token,
            })
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    cancel.cancel();
    let result = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status, crate::ScriptStatus::Cancelled);
    assert_eq!(fetcher.active.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn stat_and_exists_are_metadata_only_and_never_fill_the_cache() {
    let fetcher = Arc::new(SizingFetcher {
        fetches: AtomicUsize::new(0),
        sizes: AtomicUsize::new(0),
        blobs: HashMap::from([(cid('a'), PNG.to_vec())]),
    });
    let fs = AttachmentFs::new(fetcher.clone());
    let path = PathBuf::from(format!("/{}", cid('a')));
    assert_file_metadata(&fs, &path, PNG.len() as u64).await;
    assert_eq!(fetcher.sizes.load(Ordering::SeqCst), 2);
    assert_download_count(&fetcher, 0);
    assert_usage(&fs, 0, 0);
    // Missing blobs report NotFound, not a zero-byte file.
    let missing = PathBuf::from(format!("/{}", cid('b')));
    assert!(!fs.exists(&missing).await.unwrap());
    assert!(fs
        .stat(&missing)
        .await
        .unwrap_err()
        .to_string()
        .contains("attachment not found"));
    // Repeated script-level probes stay metadata-only.
    let mounted = attachment_path(&cid('a')).unwrap().display().to_string();
    let result = script(
        &format!("for i in 1 2 3; do test -f '{mounted}' || exit 9; done; echo probed"),
        fs,
    )
    .await;
    assert_script_stdout(&result, "probed\n");
    assert_download_count(&fetcher, 0);
}

#[tokio::test]
async fn full_cache_fails_fast_with_a_clear_message_and_no_fetch() {
    let fetcher = Arc::new(SizingFetcher {
        fetches: AtomicUsize::new(0),
        sizes: AtomicUsize::new(0),
        blobs: HashMap::from([(cid('a'), vec![0, 255, 1]), (cid('b'), vec![2, 3, 4])]),
    });
    let fs = AttachmentFs::with_limits(fetcher.clone(), FsLimits::default().max_total_bytes(3));
    let a = PathBuf::from(format!("/{}", cid('a')));
    let b = PathBuf::from(format!("/{}", cid('b')));
    assert_eq!(fs.read_file(&a).await.unwrap(), [0, 255, 1]);
    let error = fs.read_file(&b).await.unwrap_err().to_string();
    assert_cache_full(&error, 3);
    assert!(
        !error.contains("0 byte"),
        "the full-cache error must not claim a 0 byte limit: {error}"
    );
    assert_download_count(&fetcher, 1);
    // Metadata probes still work when the cache is full.
    assert_eq!(fs.stat(&b).await.unwrap().size, 3);
    assert_usage(&fs, 3, 1);
}

#[tokio::test]
async fn directory_blob_and_link_type_mismatches_fail_clearly() {
    let fetcher = Arc::new(SizingFetcher {
        fetches: AtomicUsize::new(0),
        sizes: AtomicUsize::new(0),
        blobs: HashMap::from([(cid('a'), PNG.to_vec())]),
    });
    let fs = AttachmentFs::new(fetcher.clone());
    // FileSystem methods are called relative to the mount root, so "/" is
    // the mount's directory; bashkit rewrites "/attachments" to it in scripts.
    assert!(fs
        .read_file(&PathBuf::from("/"))
        .await
        .unwrap_err()
        .to_string()
        .contains("is a directory"));
    let blob = PathBuf::from(format!("/{}", cid('a')));
    assert!(fs
        .read_dir(&blob)
        .await
        .unwrap_err()
        .to_string()
        .contains("not a directory"));
    let link = fs.read_link(&blob).await.unwrap_err();
    assert!(link.to_string().contains("attachment is not a symlink"));
    assert_eq!(
        match link {
            bashkit::Error::Io(error) => error.kind(),
            other => panic!("expected io error, got {other:?}"),
        },
        ErrorKind::InvalidInput
    );
    assert_eq!(fetcher.fetches.load(Ordering::SeqCst), 0);
    // The same mismatch surfaces through the script layer.
    let result = script("cat /attachments", AttachmentFs::new(fetcher)).await;
    assert_ne!(result.exit_code, 0);
    assert!(
        result.stderr.to_lowercase().contains("is a directory"),
        "{}",
        result.stderr
    );
    // `ls <file>` is legal POSIX (prints the name), so read_dir's mismatch is
    // asserted at the FileSystem level above.
}

#[tokio::test]
async fn cancelling_last_queued_reader_removes_its_download_lock() {
    let fs = AttachmentFs::new(FakeFetcher::png());
    let cid = cid('a');
    let first = Flight::acquire(&fs, &cid).await;
    let mut queued = Box::pin(Flight::acquire(&fs, &cid));
    // Poll once under the held lock, without relying on task scheduling or sleeps.
    let poll = std::future::poll_fn(|cx| {
        std::task::Poll::Ready(std::future::Future::poll(queued.as_mut(), cx))
    })
    .await;
    assert!(poll.is_pending());
    drop(first);
    // Cancel after the holder leaves but before the queued reader acquires.
    drop(queued);
    assert_no_download_locks(&fs, "a cancelled queued reader must remove its lock entry");
}
