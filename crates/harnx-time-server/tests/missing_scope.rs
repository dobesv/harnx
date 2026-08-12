use std::process::Command;

#[test]
fn missing_scope_names_how_to_launch_the_binary() {
    harnx_core::require_nextest();
    let output = Command::new(env!("CARGO_BIN_EXE_harnx-time-server"))
        .env_remove("HARNX_SERVER_SCOPE")
        .env("HARNX_NATS_URL", "nats://127.0.0.1:4222")
        .env("HARNX_NATS_TOKEN", "unused")
        .output()
        .expect("run harnx-time-server");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--mcp-stdio"),
        "should point at the standalone stdio mode; got: {stderr}"
    );
    assert!(
        stderr.contains("harnx-worker"),
        "should say the worker normally supplies this; got: {stderr}"
    );
}
