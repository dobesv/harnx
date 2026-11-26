//! The packages and example config harnx ships must pass `harnx validate`
//! with their tool servers started: no errors. Warnings are allowed, since
//! a bridged server whose `npx` or `uvx` can't run here, or an access rule
//! written for agents the example doesn't define, is expected. Every native
//! tool server must be found and list its tools, or the tool names using it
//! would go unchecked and the test would pass without checking them.

use std::fs;
use std::path::{Path, PathBuf};

use harnx_runtime::config_check::{check_config_dir, CheckOptions, Code, Report, Severity};

use super::EnvVarGuard;

const PACKAGES: [&str; 2] = ["pantheon", "coding"];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/<crate> sits two levels below the workspace root")
        .to_path_buf()
}

/// Copy `source` into `target`, which must not exist yet. A copy rather
/// than a symlink, so the test needs no symlink privilege on Windows.
fn copy_dir(source: &Path, target: &Path) {
    fs::create_dir_all(target).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let destination = target.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &destination);
        } else {
            fs::copy(entry.path(), &destination).unwrap();
        }
    }
}

/// Check `root` with its tool servers started, treating `ignore_tools` as
/// valid tool names.
async fn check_with_tools(root: &Path, ignore_tools: &[&str]) -> Report {
    let _config_dir = EnvVarGuard::set("HARNX_CONFIG_DIR", root);
    let stubs = tempfile::tempdir().unwrap();
    let _path = stub_bridged_runtimes(stubs.path());
    let mut options = CheckOptions::new(root.to_path_buf());
    options.check_tools = true;
    options.ignore_tools = ignore_tools.iter().map(|glob| glob.to_string()).collect();
    #[cfg(not(unix))]
    {
        // No stubs here (see stub_bridged_runtimes), so a bridged server's
        // `npx` would hold its listing for the whole default timeout or
        // reach the network. A short timeout makes it a quick warning; a
        // native server that misses it is a warning too, not a failure.
        options.list_timeout = std::time::Duration::from_secs(5);
    }
    check_config_dir(&options).await.unwrap()
}

/// The shipped configs bridge third-party MCP servers through `npx` and
/// `uvx`. Run for real, they wait out the 30 s listing timeout where there
/// is no network, and where there is one the result depends on the network
/// and on what the upstream server lists today. Stubs first on `PATH` that
/// exit 1 make every bridged server a `tool-unverified` warning at once;
/// native servers are found next to the test binary and still run.
#[cfg(unix)]
fn stub_bridged_runtimes(dir: &Path) -> Option<EnvVarGuard> {
    use std::os::unix::fs::PermissionsExt;
    for name in ["npx", "uvx"] {
        let stub = dir.join(name);
        fs::write(&stub, "#!/bin/sh\nexit 1\n").unwrap();
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(dir.to_path_buf()).chain(std::env::split_paths(&inherited)),
    )
    .unwrap();
    Some(EnvVarGuard::set("PATH", Path::new(&path)))
}

/// Windows resolves `npx` through `PATHEXT` to `.cmd` and `.exe` files, which
/// a shell-script stub can't stand in for; the caller shortens the listing
/// timeout instead.
#[cfg(not(unix))]
fn stub_bridged_runtimes(_dir: &Path) -> Option<EnvVarGuard> {
    None
}

/// Servers the check can't list from a test, whatever is built: a bridge
/// whose `npx` or `uvx` is stubbed out, and servers that need credentials or
/// a cluster.
const UNLISTABLE_HERE: [&str; 3] = [
    "harnx-mcp-bridge",
    "harnx-mcp-remote",
    "harnx-k8s-sandbox-tools",
];

/// No errors, no missing tool server command, and every `tool-unverified`
/// finding is for a server in [`UNLISTABLE_HERE`]. A missing command gets
/// `command-not-found` instead of `tool-unverified`, so it is rejected
/// here or the server's tool names would go unchecked unnoticed.
fn assert_no_errors(root: &Path, report: &Report) {
    assert!(report.summary.files_checked > 0, "{report:#?}");
    let errors = report
        .findings
        .iter()
        .filter(|finding| {
            finding.severity == Severity::Error || finding.code == Code::CommandNotFound
        })
        .count();
    assert_eq!(errors, 0, "\n{}", report.render_text());
    for finding in report
        .findings
        .iter()
        .filter(|finding| finding.code == Code::ToolUnverified)
    {
        let file = finding.file.as_deref().expect("names its server file");
        let command = server_command(&root.join(file));
        assert!(
            UNLISTABLE_HERE.contains(&command.as_str()),
            "native tool server {command} didn't list its tools (is the workspace built?): {}",
            finding.message
        );
    }
}

/// The file stem of a tool server file's `command`.
fn server_command(file: &Path) -> String {
    let content = fs::read_to_string(file).unwrap();
    let server: serde_yaml::Value = serde_yaml::from_str(&content).unwrap();
    let command = server["command"].as_str().expect("a command");
    Path::new(command)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn shipped_packages_validate_cleanly() {
    let root = workspace_root();
    let config = tempfile::tempdir().unwrap();
    for package in PACKAGES {
        let source = root.join("packages").join(package);
        copy_dir(&source, &config.path().join("packages").join(package));
    }

    // Pantheon's agents use the Kubernetes sandbox gateway's `sandbox_*`
    // tools, and the package doesn't configure the gateway: a deployment that
    // runs it does.
    let report = check_with_tools(config.path(), &["sandbox_*"]).await;

    assert_no_errors(config.path(), &report);
}

#[tokio::test]
async fn example_config_validates_cleanly() {
    let config = tempfile::tempdir().unwrap();
    let root = config.path();
    copy_dir(&workspace_root().join("example_config"), root);
    let packages = root.join("packages");
    fs::rename(
        root.join("credential-tools"),
        packages.join("credential-tools"),
    )
    .unwrap();
    fs::rename(root.join("access.example.yaml"), root.join("access.yaml")).unwrap();
    fs::rename(root.join("users.example.yaml"), root.join("users.yaml")).unwrap();

    let report = check_with_tools(root, &[]).await;

    assert_no_errors(root, &report);
}
