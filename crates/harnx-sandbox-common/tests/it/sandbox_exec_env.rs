//! What `harnx-sandbox-exec` puts in the environment of the command it runs.

#![cfg(target_os = "linux")]

use std::process::Command;

/// Enough of the system for `sh` to start inside the sandbox.
const SYSTEM_PATHS: [&str; 11] = [
    "/usr/bin",
    "/bin",
    "/lib",
    "/lib64",
    "/usr/lib",
    "/usr/lib64",
    "/etc",
    "/proc",
    "/dev",
    "/tmp",
    "/usr/share",
];

/// `harnx-sandbox-exec` with the system paths allowed, ready for a command.
fn sandboxed() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_harnx-sandbox-exec"));
    for path in SYSTEM_PATHS {
        command.arg("--exec").arg(path);
    }
    command.args(["--working-dir", "/tmp", "--"]);
    command
}

/// Nothing inside harnx's sandbox can create namespaces, so commands that
/// would nest a sandbox, such as the test runner `scripts/nextest-sandbox`,
/// need a way to tell they are inside one.
#[test]
fn marks_the_command_as_sandboxed() {
    let probe = sandboxed().arg("true").output().expect("run the probe");
    if !probe.status.success() {
        eprintln!(
            "skipping: harnx-sandbox-exec can't start a sandbox here: {}",
            String::from_utf8_lossy(&probe.stderr)
        );
        return;
    }
    let output = sandboxed()
        .args(["sh", "-c", "printf %s \"$HARNX_IN_SANDBOX\""])
        .output()
        .expect("run harnx-sandbox-exec");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "1");
}
