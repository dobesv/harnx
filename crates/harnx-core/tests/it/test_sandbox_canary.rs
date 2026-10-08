//! Fails when tests stop running inside `scripts/nextest-sandbox`, the Cargo
//! target runner `.cargo/config.toml` sets for Linux and macOS (AGENTS.md,
//! "Test sandbox"). Removing that setting, or a runner that passes
//! everything through, trips it.

use std::path::PathBuf;

#[test]
fn tests_run_inside_the_sandbox() {
    harnx_core::require_nextest();
    let mode = std::env::var("HARNX_TEST_SANDBOX").unwrap_or_default();
    if mode == "off" {
        eprintln!("HARNX_TEST_SANDBOX=off: the sandbox is switched off for this run");
        return;
    }
    assert_private_home();
    assert_no_developer_environment();
    // The light sandbox, which harnx's own sandbox gets, has no namespaces.
    #[cfg(target_os = "linux")]
    if mode != "light" {
        assert_own_namespaces();
    }
}

fn var_path(name: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(name).unwrap_or_else(|| panic!("{name} is unset")))
}

fn assert_private_home() {
    let private = var_path("XDG_CONFIG_HOME")
        .parent()
        .expect("private home")
        .to_path_buf();
    let misplaced: Vec<&str> = [
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
        "XDG_RUNTIME_DIR",
    ]
    .into_iter()
    .filter(|name| var_path(name).parent() != Some(private.as_path()))
    .collect();
    let resolved = [
        (harnx_core::config_paths::config_dir(), "XDG_CONFIG_HOME"),
        (harnx_core::config_paths::data_dir(), "XDG_DATA_HOME"),
        (harnx_core::config_paths::state_dir(), "XDG_STATE_HOME"),
    ];
    let elsewhere: Vec<&PathBuf> = resolved
        .iter()
        .filter(|(dir, xdg)| *dir != var_path(xdg).join("harnx"))
        .map(|(dir, _)| dir)
        .collect();
    assert!(misplaced.is_empty(), "outside {private:?}: {misplaced:?}");
    assert!(
        elsewhere.is_empty(),
        "harnx resolves elsewhere: {elsewhere:?}"
    );
    assert!(
        !private.starts_with(var_path("HOME")),
        "{private:?} is inside HOME"
    );
}

/// Variables that must never reach a test. This checks categories instead of
/// mirroring the runner's allowlist, which lives only in the script.
fn from_developer_environment(name: &str) -> bool {
    const SET_BY_RUNNER: [&str; 4] = [
        "HARNX_CONFIG_DIR",
        "HARNX_DATA_DIR",
        "HARNX_STATE_DIR",
        "HARNX_TEST_SANDBOX",
    ];
    const SESSION: [&str; 8] = [
        "SSH_AUTH_SOCK",
        "DBUS_SESSION_BUS_ADDRESS",
        "KUBECONFIG",
        "TMUX",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
    ];
    let upper = name.to_ascii_uppercase();
    (upper.starts_with("HARNX_") && !SET_BY_RUNNER.contains(&name))
        || upper.starts_with("GIT_")
        || SESSION.contains(&upper.as_str())
        || ["TOKEN", "SECRET", "PASSWORD", "API_KEY"]
            .iter()
            .any(|word| upper.contains(word))
}

fn assert_no_developer_environment() {
    let leaked: Vec<String> = std::env::vars_os()
        .map(|(name, _)| name.to_string_lossy().into_owned())
        .filter(|name| from_developer_environment(name))
        .collect();
    assert!(leaked.is_empty(), "reached the test: {leaked:?}");
}

/// PID 1 is bwrap's init only inside its PID namespace, and a fresh network
/// namespace has nothing but loopback.
#[cfg(target_os = "linux")]
fn assert_own_namespaces() {
    let init = std::fs::read_to_string("/proc/1/comm").expect("read /proc/1/comm");
    let devices = std::fs::read_to_string("/proc/net/dev").expect("read /proc/net/dev");
    let interfaces: Vec<&str> = devices
        .lines()
        .skip(2)
        .filter_map(|line| line.split(':').next())
        .map(str::trim)
        .collect();
    let developer_data = var_path("HOME").join(".local/share/harnx");
    let visible = std::fs::read_dir(&developer_data)
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(init.trim(), "bwrap", "PID 1 is not bwrap's init");
    assert_eq!(interfaces, ["lo"], "network interfaces");
    assert_eq!(visible, 0, "{developer_data:?} is visible");
}
