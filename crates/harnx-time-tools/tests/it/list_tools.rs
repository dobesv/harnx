use harnx_toolset::listing::ToolListing;
use std::process::{Command, Stdio};
use std::time::Duration;

/// Run the binary with `--list-tools` and no broker, failing if it hangs.
fn list(extra: &[&str]) -> ToolListing {
    let mut child = Command::new(env!("CARGO_BIN_EXE_harnx-time-tools"))
        .args(["--list-tools", "--format", "json"])
        .args(extra)
        .env_remove("HARNX_NATS_URL")
        .env_remove("HARNX_NATS_TOKEN")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn harnx-time-tools");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if child.try_wait().expect("poll child").is_some() {
            break;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("--list-tools did not exit within 10s");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output().expect("collect output");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("stdout is a ToolListing")
}

#[test]
fn time_tools_lists_json_without_nats() {
    let listing = list(&[]);
    assert_eq!(listing.server, "time");
    assert!(listing.tools.iter().any(|tool| tool.name == "wait"));
}

#[test]
fn time_tools_listing_honours_name_override() {
    let listing = list(&["--name", "clock"]);
    assert_eq!(listing.server, "clock");
}

#[test]
fn time_tools_listing_honours_enable_tool_filter() {
    let listing = list(&["--enable-tool=wait"]);
    assert_eq!(
        listing
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["wait"]
    );
}
