use anyhow::{Context, Result};
use std::fs::{create_dir_all, File, TryLockError};
use std::path::{Path, PathBuf};

pub struct SessionLock {
    _file: File,
}

impl SessionLock {
    /// Derive lock path from a session file path: session_path.with_extension("yaml.lock")
    pub fn lock_path_for(session_path: &Path) -> PathBuf {
        session_path.with_extension("yaml.lock")
    }

    fn open_lock_file(session_path: &Path) -> Result<(PathBuf, File)> {
        let lock_path = Self::lock_path_for(session_path);
        if let Some(dir) = lock_path.parent() {
            create_dir_all(dir)
                .with_context(|| format!("Failed to create session lock dir {}", dir.display()))?;
        }
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("Failed to open session lock file {}", lock_path.display()))?;
        Ok((lock_path, file))
    }

    /// Block until exclusive lock acquired. Returns Ok(lock).
    pub fn acquire(session_path: &Path) -> Result<Self> {
        let (lock_path, file) = Self::open_lock_file(session_path)?;
        file.lock()
            .with_context(|| format!("Failed to acquire session lock {}", lock_path.display()))?;
        Ok(Self { _file: file })
    }

    /// Non-blocking. Returns Ok(Some(lock)) if acquired, Ok(None) if another process holds it.
    pub fn try_acquire(session_path: &Path) -> Result<Option<Self>> {
        let (lock_path, file) = Self::open_lock_file(session_path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => Err(e).with_context(|| {
                format!("Failed to try_acquire session lock {}", lock_path.display())
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SessionLock;
    use std::path::PathBuf;
    use std::sync::{Arc, Barrier, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    fn session_path(temp_dir: &TempDir) -> PathBuf {
        temp_dir.path().join("mysession.yaml")
    }

    #[test]
    fn acquire_creates_missing_parent_dir() {
        let temp_dir = TempDir::new().unwrap();
        let session_path = temp_dir
            .path()
            .join("nonexistent_subdir")
            .join("mysession.yaml");
        let lock_path = SessionLock::lock_path_for(&session_path);

        assert!(!lock_path.parent().unwrap().exists());

        let lock = SessionLock::acquire(&session_path).unwrap();

        assert!(lock_path.parent().unwrap().exists());
        assert!(lock_path.exists());
        drop(lock);
    }

    #[test]
    fn try_acquire_returns_none_when_lock_held() {
        let temp_dir = TempDir::new().unwrap();
        let session_path = session_path(&temp_dir);

        let first = SessionLock::try_acquire(&session_path)
            .unwrap()
            .expect("first try_acquire should succeed");
        let second = SessionLock::try_acquire(&session_path).unwrap();

        assert!(
            second.is_none(),
            "second try_acquire should return None while first guard is alive"
        );
        drop(first);
    }

    #[test]
    fn drop_releases_lock_for_subsequent_try_acquire() {
        let temp_dir = TempDir::new().unwrap();
        let session_path = session_path(&temp_dir);

        let first = SessionLock::try_acquire(&session_path)
            .unwrap()
            .expect("first try_acquire should succeed");
        assert!(SessionLock::try_acquire(&session_path).unwrap().is_none());

        drop(first);

        let second = SessionLock::try_acquire(&session_path)
            .unwrap()
            .expect("try_acquire should succeed after drop releases lock");
        drop(second);
    }

    #[test]
    fn acquire_blocks_until_prior_holder_releases_lock() {
        let temp_dir = TempDir::new().unwrap();
        let session_path = session_path(&temp_dir);
        let barrier = Arc::new(Barrier::new(2));
        let markers = Arc::new(Mutex::new(Vec::<(&'static str, Instant)>::new()));

        let thread_one_path = session_path.clone();
        let thread_one_barrier = Arc::clone(&barrier);
        let thread_one_markers = Arc::clone(&markers);
        let thread_one = thread::spawn(move || {
            let _lock = SessionLock::acquire(&thread_one_path).unwrap();
            thread_one_markers
                .lock()
                .unwrap()
                .push(("thread1_acquired", Instant::now()));
            thread_one_barrier.wait();
            thread::sleep(Duration::from_millis(175));
            thread_one_markers
                .lock()
                .unwrap()
                .push(("thread1_releasing", Instant::now()));
        });

        let thread_two_path = session_path.clone();
        let thread_two_barrier = Arc::clone(&barrier);
        let thread_two_markers = Arc::clone(&markers);
        let thread_two = thread::spawn(move || {
            thread_two_barrier.wait();
            let start = Instant::now();
            let _lock = SessionLock::acquire(&thread_two_path).unwrap();
            let acquired_at = Instant::now();
            thread_two_markers
                .lock()
                .unwrap()
                .push(("thread2_acquired", acquired_at));
            acquired_at.duration_since(start)
        });

        thread_one.join().unwrap();
        let blocked_for = thread_two.join().unwrap();

        let markers = markers.lock().unwrap();
        let thread1_acquired = markers
            .iter()
            .find(|(label, _)| *label == "thread1_acquired")
            .map(|(_, at)| *at)
            .unwrap();
        let thread1_releasing = markers
            .iter()
            .find(|(label, _)| *label == "thread1_releasing")
            .map(|(_, at)| *at)
            .unwrap();
        let thread2_acquired = markers
            .iter()
            .find(|(label, _)| *label == "thread2_acquired")
            .map(|(_, at)| *at)
            .unwrap();

        assert!(thread1_acquired <= thread1_releasing);
        assert!(thread1_releasing <= thread2_acquired);
        assert!(blocked_for >= Duration::from_millis(125));
    }
}
