//! Integration test: confirm that `tcsetattr` succeeds inside a `MacSandbox`.
//!
//! This is the runtime counterpart to the `default_profile_allows_file_ioctl`
//! unit test. The unit test verifies the SBPL string contains the magic line;
//! this test fork+execs a child with a real pty as its stdin and verifies the
//! child's `tcsetattr` call actually goes through. Catches the kind of break
//! a string match wouldn't — e.g. someone moves the `file-ioctl` allow into a
//! path-scoped rule where it no longer matches the pty device.
//!
//! Single-test file by design: `MacSandbox::apply_and_spawn` applies
//! `sandbox_init` to the current process, which is irreversible. Running a
//! second sandbox-using test in the same binary would inherit the first
//! sandbox's restrictions.

#![cfg(target_os = "macos")]

use std::fs::File;
use std::os::unix::io::FromRawFd;
use std::path::Path;
use std::process::{Command, Stdio};
use std::ptr;

use harnx_sandbox_common::macos_sandbox::MacSandbox;

#[test]
fn tcsetattr_succeeds_inside_sandbox() {
    // Cargo sets this at test-build time to the absolute path of the
    // `harnx_tty_probe` binary it just compiled.
    let probe = env!("CARGO_BIN_EXE_harnx_tty_probe");
    let probe_path = Path::new(probe);
    let probe_dir = probe_path
        .parent()
        .expect("probe should not live at filesystem root");

    // Allocate a pty pair so the probe child has a real terminal as stdin.
    // Without this, `tcgetattr` would fail with ENOTTY before we get to test
    // the actually-interesting `tcsetattr`.
    let mut master_fd: libc::c_int = 0;
    let mut slave_fd: libc::c_int = 0;
    let rc = unsafe {
        libc::openpty(
            &mut master_fd,
            &mut slave_fd,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    assert_eq!(rc, 0, "openpty failed: {}", std::io::Error::last_os_error());

    // Wrap the slave fd as a File so std::process::Command can take it as stdin.
    // SAFETY: openpty just returned a fresh, owned fd. No one else holds it.
    let slave_file = unsafe { File::from_raw_fd(slave_fd) };

    let mut command = Command::new(probe);
    command.stdin(Stdio::from(slave_file));

    let mut sandbox = MacSandbox::new();
    // Grant exec on the dir containing the probe so the kernel can load it.
    sandbox
        .allow_execute_and_read(probe_dir)
        .expect("granting probe dir failed");
    // Keep the full env so the probe inherits anything Rust's runtime needs.
    sandbox.allow_full_env();

    let mut child = sandbox
        .apply_and_spawn(command)
        .expect("apply_and_spawn failed");
    let status = child.wait().expect("waiting for probe failed");

    // Hand back the master fd; the child closed its slave when it exited.
    unsafe { libc::close(master_fd) };

    assert!(
        status.success(),
        "tty_probe exited with {:?} — file-ioctl is likely being denied by the sandbox",
        status.code()
    );
}
