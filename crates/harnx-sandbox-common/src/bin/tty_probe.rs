//! Test helper for `tests/macos_tty.rs`.
//!
//! Reads termios state from stdin and writes it back unchanged. This is the
//! minimal exercise of `tcsetattr` — the operation that fails silently inside
//! a birdcage-only sandbox because `file-ioctl` isn't granted. Used to verify
//! that `MacSandbox`'s default profile (which adds `(allow file-ioctl)`)
//! actually permits raw-mode entry at runtime, not just in the generated
//! profile string.
//!
//! Exit codes:
//!   0 — tcgetattr and tcsetattr both succeeded
//!   1 — tcgetattr failed (likely stdin not a tty)
//!   2 — tcsetattr failed (likely sandbox denying file-ioctl)

#[cfg(target_os = "macos")]
fn main() {
    use std::os::unix::io::AsRawFd;

    let fd = std::io::stdin().as_raw_fd();
    let mut t: libc::termios = unsafe { std::mem::zeroed() };

    let rc = unsafe { libc::tcgetattr(fd, &mut t) };
    if rc != 0 {
        eprintln!("tcgetattr failed: {}", std::io::Error::last_os_error());
        std::process::exit(1);
    }

    let rc = unsafe { libc::tcsetattr(fd, libc::TCSANOW, &t) };
    if rc != 0 {
        eprintln!("tcsetattr failed: {}", std::io::Error::last_os_error());
        std::process::exit(2);
    }

    println!("tcsetattr OK");
}

#[cfg(not(target_os = "macos"))]
fn main() {
    // Helper is only meaningful on macOS — other platforms exit cleanly so
    // cargo build doesn't fail when this crate is compiled cross-platform.
    std::process::exit(0);
}
