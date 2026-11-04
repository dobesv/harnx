use super::*;

/// Returns bytes even when they exceed the fetch limit, so the filesystem's
/// post-fetch checks must protect against stale metadata and broken fetchers.
struct MetadataFetcher {
    started: tokio::sync::mpsc::UnboundedSender<u64>,
    release: tokio::sync::Semaphore,
    reported_size: Option<u64>,
    actual_size: usize,
}

#[async_trait]
impl AttachmentFetcher for MetadataFetcher {
    async fn fetch(&self, _cid: &str, max_bytes: u64) -> io::Result<Vec<u8>> {
        self.started.send(max_bytes).unwrap();
        self.release.acquire().await.unwrap().forget();
        Ok(vec![7; self.actual_size])
    }

    async fn size(&self, _cid: &str) -> io::Result<Option<u64>> {
        Ok(self.reported_size)
    }
}

fn readers(fs: &Arc<AttachmentFs>) -> Vec<tokio::task::JoinHandle<bashkit::Result<Vec<u8>>>> {
    ('a'..='t')
        .map(|letter| {
            let fs = fs.clone();
            tokio::spawn(async move {
                fs.read_file(&PathBuf::from(format!("/{}", cid(letter))))
                    .await
            })
        })
        .collect()
}

async fn reservations(rx: &mut tokio::sync::mpsc::UnboundedReceiver<u64>, expected: u64) {
    for _ in 0..20 {
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("all twenty reads must start before any download settles"),
            Some(expected)
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn twenty_concurrent_small_reads_reserve_metadata_size() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let fetcher = Arc::new(MetadataFetcher {
        started: tx,
        release: tokio::sync::Semaphore::new(0),
        reported_size: Some(3),
        actual_size: 3,
    });
    // Default limits admit only ten max-file-size claims, but all twenty
    // metadata-sized claims fit. Keep every download in flight until checked.
    let fs = Arc::new(AttachmentFs::new(fetcher.clone()));
    let readers = readers(&fs);
    reservations(&mut rx, 3).await;
    assert_eq!(fs.budget.load(Ordering::Relaxed), 60);
    assert_usage(&fs, 0, 0);
    fetcher.release.add_permits(20);
    assert_eq!(download_counts(readers, 3, 100_000_000).await, (20, 0));
    assert_usage(&fs, 60, 20);
    assert_settled_budget(&fs);
    assert_no_download_locks(&fs, "completed downloads must remove their locks");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_underreported_blobs_cannot_overcommit_and_release_claims() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let fetcher = Arc::new(MetadataFetcher {
        started: tx,
        release: tokio::sync::Semaphore::new(0),
        reported_size: Some(1),
        actual_size: 3,
    });
    let fs = Arc::new(AttachmentFs::with_limits(
        fetcher.clone(),
        FsLimits::default().max_total_bytes(20),
    ));
    let readers = readers(&fs);
    reservations(&mut rx, 1).await;
    assert_eq!(fs.budget.load(Ordering::Relaxed), 20);
    fetcher.release.add_permits(20);
    for reader in readers {
        let error = tokio::time::timeout(Duration::from_secs(2), reader)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("exceeds reserved size of 1 bytes"));
    }
    assert_usage(&fs, 0, 0);
    assert_settled_budget(&fs);
    assert_no_download_locks(&fs, "rejected downloads must remove their locks");
}

#[tokio::test]
async fn metadata_reservations_preserve_file_and_cache_bounds() {
    for (reported, file_limit, total_limit, expected) in [
        (Some(4), 3, 20, 3),
        (Some(4), 10, 3, 3),
        (Some(0), 10, 20, 0),
    ] {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let fetcher = Arc::new(MetadataFetcher {
            started: tx,
            release: tokio::sync::Semaphore::new(1),
            reported_size: reported,
            actual_size: 4,
        });
        let fs = AttachmentFs::with_limits(
            fetcher,
            FsLimits::default()
                .max_file_size(file_limit)
                .max_total_bytes(total_limit),
        );
        let error = fs
            .read_file(&PathBuf::from(format!("/{}", cid('a'))))
            .await
            .unwrap_err();
        assert_eq!(rx.try_recv().unwrap(), expected);
        assert!(error
            .to_string()
            .contains(&format!("exceeds reserved size of {expected} bytes")));
        assert_usage(&fs, 0, 0);
        assert_settled_budget(&fs);
    }
}

#[tokio::test]
async fn overreported_and_empty_metadata_settle_to_actual_bytes() {
    for (reported, actual, expected) in [(Some(5), 3, 5), (Some(0), 0, 0)] {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let fetcher = Arc::new(MetadataFetcher {
            started: tx,
            release: tokio::sync::Semaphore::new(1),
            reported_size: reported,
            actual_size: actual,
        });
        let fs = AttachmentFs::with_limits(fetcher, FsLimits::default().max_file_size(10));
        let path = PathBuf::from(format!("/{}", cid('a')));
        assert_eq!(fs.read_file(&path).await.unwrap(), vec![7; actual]);
        assert_eq!(rx.try_recv().unwrap(), expected);
        assert_usage(&fs, actual as u64, 1);
        assert_settled_budget(&fs);
        assert_eq!(fs.read_file(&path).await.unwrap(), vec![7; actual]);
        assert!(rx.try_recv().is_err(), "cache hits must not fetch again");
    }
}

#[tokio::test]
async fn missing_metadata_returns_not_found_even_when_cache_is_full() {
    for full in [false, true] {
        let fetcher = Arc::new(SizingFetcher {
            fetches: AtomicUsize::new(0),
            sizes: AtomicUsize::new(0),
            blobs: HashMap::from([(cid('a'), vec![7; 3])]),
        });
        let fs = AttachmentFs::with_limits(
            fetcher.clone(),
            FsLimits::default().max_total_bytes(3).max_file_count(1),
        );
        let (bytes, files) = if full {
            fs.read_file(&PathBuf::from(format!("/{}", cid('a'))))
                .await
                .unwrap();
            (3, 1)
        } else {
            (0, 0)
        };
        let missing_cid = cid('b');
        let error = fs
            .read_file(&PathBuf::from(format!("/{missing_cid}")))
            .await
            .unwrap_err();
        let bashkit::Error::Io(error) = error else {
            panic!("expected io error, got {error:?}");
        };
        assert_eq!(error.kind(), ErrorKind::NotFound);
        assert_eq!(
            error.to_string(),
            format!("attachment not found: {missing_cid}")
        );
        assert_download_count(&fetcher, files as usize);
        assert_usage(&fs, bytes, files);
        assert_settled_budget(&fs);
        assert_no_download_locks(&fs, "missing metadata must remove its lock");
    }
}

struct MetadataErrorFetcher {
    probes: AtomicUsize,
    fetches: AtomicUsize,
}

#[async_trait]
impl AttachmentFetcher for MetadataErrorFetcher {
    async fn fetch(&self, _cid: &str, _max_bytes: u64) -> io::Result<Vec<u8>> {
        self.fetches.fetch_add(1, Ordering::SeqCst);
        Ok(vec![7; 3])
    }

    async fn size(&self, _cid: &str) -> io::Result<Option<u64>> {
        if self.probes.fetch_add(1, Ordering::SeqCst) == 0 {
            Err(io::Error::new(
                ErrorKind::PermissionDenied,
                "metadata probe denied",
            ))
        } else {
            Ok(Some(3))
        }
    }
}

#[tokio::test]
async fn metadata_error_leaves_no_claim_or_lock_and_can_retry() {
    let fetcher = Arc::new(MetadataErrorFetcher {
        probes: AtomicUsize::new(0),
        fetches: AtomicUsize::new(0),
    });
    let fs = AttachmentFs::new(fetcher.clone());
    let path = PathBuf::from(format!("/{}", cid('a')));
    let error = fs.read_file(&path).await.unwrap_err();
    let bashkit::Error::Io(error) = error else {
        panic!("expected io error, got {error:?}");
    };
    assert_eq!(error.kind(), ErrorKind::PermissionDenied);
    assert_eq!(error.to_string(), "metadata probe denied");
    assert_eq!(fetcher.fetches.load(Ordering::SeqCst), 0);
    assert_usage(&fs, 0, 0);
    assert_settled_budget(&fs);
    assert_no_download_locks(&fs, "failed metadata probe must remove its lock");
    assert_eq!(fs.read_file(&path).await.unwrap(), [7; 3]);
    assert_eq!(fetcher.fetches.load(Ordering::SeqCst), 1);
    assert_usage(&fs, 3, 1);
    assert_settled_budget(&fs);
    assert_no_download_locks(&fs, "retried metadata probe must remove its lock");
}
