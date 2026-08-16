//! A process that never installed a logger discards its children's output
//! rather than handing them an inherited pipe.
//!
//! Its own test binary: `child_output_sink` reads a process-global `OnceLock`,
//! and this case is the one where that lock must stay unset.

use std::process::Command;

use harnx_core::logging;

#[test]
fn without_init_children_are_sent_to_the_null_device() {
    assert!(logging::current().is_none(), "no logger may be installed");

    // Reading stdout inside the child proves the descriptor is /dev/null rather
    // than an inherited pipe: a pipe read would block, a null read returns EOF.
    let output = Command::new("sh")
        .args(["-c", "echo discard me; head -c 1 /dev/stdout; exit 0"])
        .stdout(logging::child_output_sink())
        .stderr(logging::child_output_sink())
        .status()
        .expect("run child");
    assert!(output.success());
}
