//! Combined stdout/stderr capture for process-isolated nextest tests.

#[cfg(unix)]
pub fn capture_output(action: impl FnOnce()) -> String {
    use std::io::{stdout, Read, Seek, SeekFrom, Write};
    use std::os::fd::AsRawFd;

    struct RedirectGuard {
        stdout: i32,
        stderr: i32,
    }

    impl Drop for RedirectGuard {
        fn drop(&mut self) {
            // SAFETY: Restores the saved stdout/stderr file descriptors and closes the temp copies.
            // Sound because nextest runs each test in an isolated process, so there's no concurrent
            // FD access, and this is the only code touching these descriptors.
            unsafe {
                libc::dup2(self.stdout, libc::STDOUT_FILENO);
                libc::dup2(self.stderr, libc::STDERR_FILENO);
                libc::close(self.stdout);
                libc::close(self.stderr);
            }
        }
    }

    stdout().flush().unwrap();
    std::io::stderr().flush().unwrap();
    let mut captured = tempfile::tempfile().unwrap();
    // SAFETY: Duplicates stdout/stderr FDs to preserve them for later restoration.
    // Sound because nextest runs each test in an isolated process, so no concurrent FD access.
    let saved_stdout = unsafe { libc::dup(libc::STDOUT_FILENO) };
    let saved_stderr = unsafe { libc::dup(libc::STDERR_FILENO) };
    assert!(saved_stdout >= 0 && saved_stderr >= 0);
    // SAFETY: Redirects stdout/stderr to the temp file for capture. Sound for the same reason:
    // process-isolated tests mean no other code is touching these FDs concurrently.
    assert_eq!(
        unsafe { libc::dup2(captured.as_raw_fd(), libc::STDOUT_FILENO) },
        libc::STDOUT_FILENO
    );
    assert_eq!(
        unsafe { libc::dup2(captured.as_raw_fd(), libc::STDERR_FILENO) },
        libc::STDERR_FILENO
    );
    let guard = RedirectGuard {
        stdout: saved_stdout,
        stderr: saved_stderr,
    };

    action();
    stdout().flush().unwrap();
    std::io::stderr().flush().unwrap();
    drop(guard);

    captured.seek(SeekFrom::Start(0)).unwrap();
    let mut output = String::new();
    captured.read_to_string(&mut output).unwrap();
    output
}
