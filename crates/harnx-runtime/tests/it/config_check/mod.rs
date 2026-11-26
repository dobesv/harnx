//! `harnx validate`'s checks, called as a library against fixture
//! directories each test creates for itself.

mod agents;
mod references;
mod report;
mod shipped;
mod silent_drops;
mod tool_names;
mod unknown_keys;

use std::fs;
use std::path::Path;

use harnx_runtime::config_check::{
    check_config_dir, discover, discover_sources, CheckOptions, Code, ConfigSources, Finding,
    Report,
};

/// Sets an environment variable until dropped, then puts it back.
struct EnvVarGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &Path) -> Self {
        let previous = std::env::var_os(key);
        // SAFETY: nextest gives each test its own process, so nothing else
        // mutates the environment concurrently.
        unsafe { std::env::set_var(key, value) };
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        // SAFETY: see EnvVarGuard::set.
        unsafe {
            match &self.previous {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

fn write(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// Check `root` the way `harnx validate` does: with `HARNX_CONFIG_DIR`
/// pointing at it, since the loader finds packages through that variable.
async fn check(root: &Path) -> Report {
    // nextest runs each test in its own process, so nothing else reads the
    // environment concurrently.
    let _config_dir = EnvVarGuard::set("HARNX_CONFIG_DIR", root);
    check_config_dir(&CheckOptions::new(root.to_path_buf()))
        .await
        .unwrap()
}

/// Check a directory holding `files`, each a path and its content.
async fn check_holding(files: &[(&str, &str)]) -> Report {
    let dir = tempfile::tempdir().unwrap();
    for (path, content) in files {
        write(dir.path(), path, content);
    }
    check(dir.path()).await
}

fn with_code(report: &Report, code: Code) -> Vec<&Finding> {
    report.findings.iter().filter(|f| f.code == code).collect()
}

#[test]
fn discover_lists_files_by_kind() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "config.yaml", "stream: true\n");
    write(root, "clients/openai.yaml", "type: openai\n");
    write(root, "clients/notes.txt", "not a client\n");
    write(root, "agents/helper.md", "hello\n");
    write(root, "agents/shared/fragment.md", "shared text\n");
    write(root, "packages/pkg/agents/worker.md", "hi\n");
    write(root, "packages/pkg/bash_tools/gh.yaml", "name: gh\n");
    write(root, "packages/pkg/package.yaml", "name: pkg\n");
    write(root, "packages/pkg.patch.yaml", "agents: {}\n");
    write(root, "packages/.hidden/agents/x.md", "hidden\n");

    let layout = discover(root).unwrap();

    assert_eq!(layout.config_file, Some(root.join("config.yaml")));
    assert_eq!(layout.clients, vec![root.join("clients/openai.yaml")]);
    assert_eq!(layout.agents, vec![root.join("agents/helper.md")]);
    assert_eq!(layout.packages.len(), 1);
    let package = &layout.packages[0];
    assert_eq!(package.name, "pkg");
    assert_eq!(
        package.agents,
        vec![root.join("packages/pkg/agents/worker.md")]
    );
    assert_eq!(
        package.bash_tools,
        vec![root.join("packages/pkg/bash_tools/gh.yaml")]
    );
    assert_eq!(
        package.patch_file,
        Some(root.join("packages/pkg.patch.yaml"))
    );
    assert_eq!(
        package.metadata,
        Some(root.join("packages/pkg/package.yaml"))
    );
    assert_eq!(package.manifest, None);
}

#[test]
fn discover_fails_for_a_missing_dir() {
    let dir = tempfile::tempdir().unwrap();

    assert!(discover(&dir.path().join("absent")).is_err());
}

#[test]
fn discover_names_a_file_given_as_the_dir() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "config.yaml", "stream: true\n");

    let error = discover(&dir.path().join("config.yaml")).unwrap_err();

    assert!(
        error.to_string().contains("is not a directory"),
        "{error:#}"
    );
}

#[test]
fn discover_sources_reads_each_kind_from_its_own_dir() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("custom");
    let home = dir.path().join("home");
    write(&root, "my.yaml", "stream: true\n");
    write(&root, "agents/helper.md", "hello\n");
    write(
        &home,
        "agents/ignored.md",
        "not read: agents sit beside the config file\n",
    );
    write(&home, "packages/pkg/agents/worker.md", "hi\n");
    write(&home, "users.yaml", "users: {}\n");
    let sources = ConfigSources {
        root: root.clone(),
        config_file: root.join("my.yaml"),
        home: home.clone(),
        macros_dir: home.join("macros"),
    };

    let layout = discover_sources(&sources).unwrap();

    assert_eq!(layout.root, root);
    assert_eq!(layout.config_file, Some(root.join("my.yaml")));
    assert_eq!(layout.agents, vec![root.join("agents/helper.md")]);
    assert_eq!(layout.packages.len(), 1);
    assert_eq!(layout.users, Some(home.join("users.yaml")));
}

#[tokio::test]
async fn check_reports_a_broken_config_once_as_parse_error() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "config.yaml", "model: [\n");

    let report = check(dir.path()).await;

    assert_eq!(report.findings.len(), 1, "{:?}", report.findings);
    let finding = &report.findings[0];
    assert_eq!(finding.code, Code::ParseError);
    assert_eq!(finding.file.as_deref(), Some(Path::new("config.yaml")));
    assert_eq!(report.exit_code(), 1);
}

#[tokio::test]
async fn check_fails_for_an_invalid_ignore_glob() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "config.yaml", "stream: true\n");
    let _config_dir = EnvVarGuard::set("HARNX_CONFIG_DIR", dir.path());
    let mut options = CheckOptions::new(dir.path().to_path_buf());
    options.ignore_tools = vec!["a[".to_string()];

    let error = check_config_dir(&options).await.unwrap_err();

    assert!(format!("{error:#}").contains("'a['"), "{error:#}");
}

#[tokio::test]
async fn only_nats_servers_is_not_empty() {
    let report =
        check_holding(&[("nats_servers/remote.yaml", "url: nats://localhost:4222\n")]).await;

    assert!(
        with_code(&report, Code::EmptyConfig).is_empty(),
        "{report:#?}"
    );
}

#[tokio::test]
async fn an_empty_dir_is_empty() {
    let report = check_holding(&[]).await;

    assert_eq!(
        with_code(&report, Code::EmptyConfig).len(),
        1,
        "{report:#?}"
    );
}

/// A subdirectory the loaders can't list reads as holding nothing, which
/// would let the check pass without looking at it. A file where `agents/`
/// should be fails to list the same way on every Unix, without depending on
/// whether the test runs as root.
#[cfg(unix)]
#[test]
fn discover_fails_for_an_unlistable_subdir() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "config.yaml", "stream: true\n");
    write(dir.path(), "agents", "not a directory\n");

    let error = discover(dir.path()).unwrap_err();

    assert!(format!("{error:#}").contains("agents"), "{error:#}");
}

#[tokio::test]
async fn check_fails_when_the_loaders_read_another_dir() {
    let checked = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    write(checked.path(), "config.yaml", "stream: true\n");
    write(other.path(), "config.yaml", "stream: true\n");
    let _config_dir = EnvVarGuard::set("HARNX_CONFIG_DIR", checked.path());
    let _config_file = EnvVarGuard::set("HARNX_CONFIG_FILE", &other.path().join("config.yaml"));

    let error = check_config_dir(&CheckOptions::new(checked.path().to_path_buf()))
        .await
        .unwrap_err();

    let message = format!("{error:#}");
    assert!(message.contains("HARNX_CONFIG_FILE"), "{message}");
    assert!(
        message.contains(&other.path().display().to_string()),
        "{message}"
    );
}
