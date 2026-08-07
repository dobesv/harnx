//! Advisory file locking for the local worker and shared NATS server elections.

use std::fs::File;
use std::fs::TryLockError;
use std::io;

/// Take the exclusive lock if it is free, reporting contention as `Ok(false)`.
///
/// `try_lock` returns one `Err` for two outcomes that mean opposite things to
/// an election. Another process already holding the lock is the ordinary path:
/// this process loses and becomes a follower. An I/O error means the attempt
/// never resolved, and treating it as a loss would leave the caller waiting on
/// an owner that does not exist.
pub(crate) fn try_lock_exclusive(file: &File) -> io::Result<bool> {
    match file.try_lock() {
        Ok(()) => Ok(true),
        Err(TryLockError::WouldBlock) => Ok(false),
        Err(TryLockError::Error(err)) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_free_lock_is_acquired_and_a_held_one_reports_contention() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("election.lock");
        let held = File::create(&path).expect("create lock file");
        assert!(try_lock_exclusive(&held).expect("first attempt"));

        // A second handle to the same file stands in for the second process.
        let contender = File::open(&path).expect("reopen lock file");
        assert!(!try_lock_exclusive(&contender).expect("contended attempt"));

        held.unlock().expect("release lock");
        assert!(try_lock_exclusive(&contender).expect("attempt after release"));
    }
}
