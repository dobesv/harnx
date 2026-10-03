use std::process::Command;

fn cli_command() -> Command {
    let command = Command::new(env!("CARGO_BIN_EXE_harnx"));
    // Reproduce Windows' 1 MiB executable stack on Linux, where the default
    // otherwise hides oversized async dispatch frames. Windows runs unchanged.
    #[cfg(target_os = "linux")]
    let command = unsafe {
        use std::os::unix::process::CommandExt;
        let mut command = command;
        command.pre_exec(|| {
            let limit = libc::rlimit {
                rlim_cur: 1024 * 1024,
                rlim_max: 1024 * 1024,
            };
            if libc::setrlimit(libc::RLIMIT_STACK, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
        command
    };
    command
}

#[test]
fn invalid_tool_json_is_parseable_failure_before_config_or_worker_startup() {
    let dir = tempfile::tempdir().unwrap();
    // A broken config proves validation happens before runtime initialization.
    std::fs::write(dir.path().join("config.yaml"), "[: broken YAML").unwrap();
    for raw in ["{bad", "null", "[]", "1", "true", "\"string\""] {
        for agent in [None, Some("nonexistent@not-a-cluster")] {
            let mut command = cli_command();
            command
                .env("HARNX_CONFIG_DIR", dir.path())
                .env("HARNX_STATE_DIR", dir.path());
            if let Some(agent) = agent {
                command.args(["--agent", agent]);
            }
            let output = command
                .args(["call", "tool", "fs_read", raw, "--json"])
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(1));
            let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(value["isError"], true);
            assert!(value["error"].as_str().unwrap().contains(if raw == "{bad" {
                "Invalid tool argument JSON"
            } else {
                "Tool arguments must be a JSON object"
            }));
            assert!(String::from_utf8_lossy(&output.stderr).contains("error:"));
            assert!(!dir.path().join("sessions").exists());
        }
    }
}

#[test]
fn tool_config_failures_have_json_stdout_and_human_stderr() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.yaml"), "[: broken YAML").unwrap();
    for args in [
        vec!["list", "tools", "--json"],
        vec!["info", "tool", "fs_read", "--json"],
        vec!["call", "tool", "fs_read", "{}", "--json"],
    ] {
        let output = cli_command()
            .env("HARNX_CONFIG_DIR", dir.path())
            .env("HARNX_STATE_DIR", dir.path())
            .args(args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["isError"], true);
        assert!(value["error"].as_str().unwrap().contains("config"));
        assert!(String::from_utf8_lossy(&output.stderr).contains("error:"));
    }
}
