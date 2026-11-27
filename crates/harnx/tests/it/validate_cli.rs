//! `harnx validate` run as a binary: exit codes, output formats, `--package`
//! mode, and that checking never starts a broker.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

const AGENT: &str = "---\ndescription: Helps\n---\nYou are helpful.\n";

fn write(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// Run `harnx validate <args>` with harnx's own directories in `home`, so
/// nothing the command writes lands outside the test's temp dir.
fn validate(home: &Path, args: &[&str]) -> Output {
    validate_with_env(home, args, &[])
}

fn validate_with_env(home: &Path, args: &[&str], env: &[(&str, &Path)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_harnx"));
    command
        .arg("validate")
        .args(args)
        .env("HARNX_CONFIG_DIR", home.join("config"))
        .env("HARNX_DATA_DIR", home.join("data"))
        .env("HARNX_STATE_DIR", home.join("state"))
        .env_remove("HARNX_CONFIG_FILE")
        .env_remove("HARNX_MACROS_DIR")
        .env_remove("HARNX_NATS_URL")
        .env_remove("HARNX_NATS_SERVER");
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().expect("run harnx validate")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn describe(output: &Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        stdout(output),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Run `harnx validate <args> --format json`, expect exit code `code` and
/// return the report.
fn validate_json(home: &Path, args: &[&str], code: i32) -> serde_json::Value {
    let mut args = args.to_vec();
    args.extend(["--format", "json"]);
    let output = validate(home, &args);
    assert_eq!(output.status.code(), Some(code), "{}", describe(&output));
    serde_json::from_slice(&output.stdout).expect("JSON report")
}

/// A config directory with two files that don't parse: a tool server and a
/// client.
fn two_broken_files(root: &Path) {
    clean_config(root);
    write(root, "tool_servers/broken.yaml", "command: [\n");
    write(root, "clients/broken.yaml", "type: [\n");
}

fn clean_config(root: &Path) {
    write(root, "config.yaml", "stream: true\n");
    write(root, "agents/helper.md", AGENT);
}

#[test]
fn validate_clean_dir_exits_zero() {
    let home = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    clean_config(config.path());

    let output = validate(
        home.path(),
        &["--config-dir", config.path().to_str().unwrap()],
    );

    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    let text = stdout(&output);
    let last = text.lines().last().unwrap_or_default();
    assert!(
        last.contains("0 errors, 0 warnings"),
        "summary is the last line: {}",
        describe(&output)
    );
}

#[test]
fn validate_broken_yaml_exits_one() {
    let home = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    write(config.path(), "config.yaml", "model: [\n");
    let dir = config.path().to_str().unwrap();

    let text = validate(home.path(), &["--config-dir", dir]);
    assert_eq!(text.status.code(), Some(1), "{}", describe(&text));
    assert!(
        stdout(&text).contains("error[parse-error]: config.yaml:1: "),
        "{}",
        describe(&text)
    );

    let json = validate(home.path(), &["--config-dir", dir, "--format", "json"]);
    assert_eq!(json.status.code(), Some(1), "{}", describe(&json));
    let value: serde_json::Value = serde_json::from_slice(&json.stdout).expect("JSON report");
    assert_eq!(value["findings"][0]["code"], "parse-error", "{value}");
    assert_eq!(value["findings"][0]["file"], "config.yaml", "{value}");
}

#[test]
fn validate_missing_dir_exits_two() {
    let home = tempfile::tempdir().unwrap();
    let missing = home.path().join("nonexistent");

    let output = validate(home.path(), &["--config-dir", missing.to_str().unwrap()]);

    assert_eq!(output.status.code(), Some(2), "{}", describe(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("nonexistent"),
        "{}",
        describe(&output)
    );
}

#[cfg(unix)]
#[test]
fn validate_unreadable_dir_exits_two() {
    use std::os::unix::fs::PermissionsExt;

    let home = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    clean_config(config.path());
    let set_mode = |mode| {
        fs::set_permissions(config.path(), fs::Permissions::from_mode(mode)).unwrap();
    };
    set_mode(0o000);
    if fs::read_dir(config.path()).is_ok() {
        // Root, or a sandbox running as root, reads it anyway: nothing to test.
        set_mode(0o755);
        eprintln!("skipping: the directory is still readable after chmod 000");
        return;
    }

    let output = validate(
        home.path(),
        &["--config-dir", config.path().to_str().unwrap()],
    );
    set_mode(0o755);

    assert_eq!(output.status.code(), Some(2), "{}", describe(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Cannot read config directory"),
        "{}",
        describe(&output)
    );
}

#[test]
fn validate_follows_harnx_config_file() {
    let home = tempfile::tempdir().unwrap();
    let custom = tempfile::tempdir().unwrap();
    write(custom.path(), "my.yaml", "model: [\n");
    write(custom.path(), "agents/helper.md", AGENT);

    let output = validate_with_env(
        home.path(),
        &[],
        &[("HARNX_CONFIG_FILE", &custom.path().join("my.yaml"))],
    );

    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    assert!(
        stdout(&output).contains("error[parse-error]: my.yaml:1: "),
        "{}",
        describe(&output)
    );
    assert!(
        stdout(&output).contains("2 files checked"),
        "the config file and the agent beside it: {}",
        describe(&output)
    );
}

#[test]
fn validate_package_conflicts_with_config_dir() {
    let home = tempfile::tempdir().unwrap();

    let output = validate(
        home.path(),
        &[
            "--config-dir",
            "a",
            "--package",
            home.path().to_str().unwrap(),
        ],
    );

    assert_eq!(output.status.code(), Some(2), "{}", describe(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("cannot be used with"),
        "{}",
        describe(&output)
    );
}

#[test]
fn validate_empty_dir_warns() {
    let home = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let dir = config.path().to_str().unwrap();

    let lenient = validate(home.path(), &["--config-dir", dir]);
    assert_eq!(lenient.status.code(), Some(0), "{}", describe(&lenient));
    assert!(
        stdout(&lenient).starts_with("warning[empty-config]: "),
        "{}",
        describe(&lenient)
    );

    let json = validate(home.path(), &["--config-dir", dir, "--format", "json"]);
    let value: serde_json::Value = serde_json::from_slice(&json.stdout).expect("JSON report");
    assert_eq!(value["findings"][0]["code"], "empty-config", "{value}");

    let strict = validate(home.path(), &["--config-dir", dir, "--strict"]);
    assert_eq!(strict.status.code(), Some(1), "{}", describe(&strict));
}

#[test]
fn validate_package_mode() {
    let home = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    let package = source.path().join("mypkg");
    write(&package, "agents/helper.md", AGENT);

    let output = validate(home.path(), &["--package", package.to_str().unwrap()]);

    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert!(
        !stdout(&output)
            .lines()
            .any(|line| line.starts_with("warning")),
        "an installed package is something to check: {}",
        describe(&output)
    );
}

#[test]
fn validate_package_mode_reports_paths_inside_the_package() {
    let home = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    let package = source.path().join("mypkg");
    write(&package, "agents/helper.md", AGENT);
    write(&package, "tool_servers/broken.yaml", "command: [\n");

    let output = validate(
        home.path(),
        &["--package", package.to_str().unwrap(), "--format", "json"],
    );

    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON report");
    let finding = &value["findings"][0];
    assert_eq!(finding["code"], "parse-error", "{value}");
    assert_eq!(
        finding["file"], "tool_servers/broken.yaml",
        "the file is named inside the package: {value}"
    );
    let message = finding["message"].as_str().unwrap();
    assert!(
        !message.contains(std::env::temp_dir().to_str().unwrap()),
        "message doesn't name the temporary config dir: {message}"
    );
}

#[test]
fn validate_does_not_start_broker() {
    let home = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    clean_config(config.path());
    write(
        config.path(),
        "nats_servers/remote.yaml",
        "url: nats://127.0.0.1:1\nagents: []\n",
    );

    let output = validate(
        home.path(),
        &["--config-dir", config.path().to_str().unwrap()],
    );

    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    let data = home.path().join("data");
    assert!(
        !data.join("nats").exists(),
        "validate must not start a broker under {}",
        data.display()
    );
}

#[test]
fn validate_package_mode_warns_for_models_on_the_installers_clients() {
    let home = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    let package = source.path().join("mypkg");
    write(
        &package,
        "agents/helper.md",
        "---\nmodel: /openai:gpt-x\nmodel_fallbacks: [other/c:gpt-y]\n---\nYou are helpful.\n",
    );

    let output = validate(
        home.path(),
        &["--package", package.to_str().unwrap(), "--format", "json"],
    );

    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON report");
    let findings = value["findings"].as_array().unwrap();
    let severity_of = |key: &str| {
        findings
            .iter()
            .find(|f| f["code"] == "model-unresolved" && f["key_path"] == key)
            .map(|f| f["severity"].clone())
    };
    assert_eq!(severity_of("model"), Some("warning".into()), "{value}");
    assert_eq!(
        severity_of("model_fallbacks"),
        Some("error".into()),
        "a model on another package's client is still an error: {value}"
    );
}

#[test]
fn validate_config_dir_errors_for_models_on_unconfigured_top_level_clients() {
    let home = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    write(
        config.path(),
        "packages/mypkg/agents/helper.md",
        "---\nmodel: /openai:gpt-x\n---\nYou are helpful.\n",
    );

    let output = validate(
        home.path(),
        &["--config-dir", config.path().to_str().unwrap()],
    );

    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    assert!(
        stdout(&output).contains("error[model-unresolved]"),
        "{}",
        describe(&output)
    );
}

#[test]
fn validate_allow_code_drops_it_everywhere() {
    let home = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    two_broken_files(config.path());
    let dir = config.path().to_str().unwrap();

    let value = validate_json(
        home.path(),
        &["--config-dir", dir, "--allow", "parse-error"],
        0,
    );

    assert_eq!(value["findings"], serde_json::json!([]), "{value}");
    assert_eq!(value["summary"]["suppressed"], 2, "{value}");
}

#[test]
fn validate_allow_code_and_glob_drops_it_in_matching_files() {
    let home = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    two_broken_files(config.path());
    let dir = config.path().to_str().unwrap();

    let value = validate_json(
        home.path(),
        &["--config-dir", dir, "--allow=parse-error:tool_servers/*"],
        1,
    );

    let findings = value["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1, "{value}");
    assert_eq!(findings[0]["file"], "clients/broken.yaml", "{value}");
    assert_eq!(value["summary"]["suppressed"], 1, "{value}");
}

#[test]
fn validate_allow_globs_match_package_relative_paths() {
    let home = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    let mut packages = Vec::new();
    for name in ["one", "two"] {
        let package = source.path().join(name);
        write(&package, "agents/helper.md", AGENT);
        write(&package, "tool_servers/broken.yaml", "command: [\n");
        packages.push(package.to_str().unwrap().to_string());
    }

    let value = validate_json(
        home.path(),
        &[
            "--package",
            &packages[0],
            "--package",
            &packages[1],
            "--allow",
            "parse-error:one/tool_servers/*",
        ],
        1,
    );

    let findings = value["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1, "{value}");
    assert_eq!(
        findings[0]["file"], "two/tool_servers/broken.yaml",
        "{value}"
    );
}

#[test]
fn validate_allow_unknown_code_exits_two() {
    let home = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    clean_config(config.path());

    let output = validate(
        home.path(),
        &[
            "--config-dir",
            config.path().to_str().unwrap(),
            "--allow",
            "tool-unverifed",
        ],
    );

    assert_eq!(output.status.code(), Some(2), "{}", describe(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown finding code 'tool-unverifed'"),
        "{}",
        describe(&output)
    );
}

#[test]
fn validate_invalid_ignore_tool_glob_exits_two() {
    let home = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    clean_config(config.path());

    let output = validate(
        home.path(),
        &[
            "--config-dir",
            config.path().to_str().unwrap(),
            "--ignore-tool",
            "a[",
        ],
    );

    assert_eq!(output.status.code(), Some(2), "{}", describe(&output));
}

/// Runtime discovery names a package after its entry under `packages/`,
/// whatever that entry links to, so a package given through a link is
/// staged under the link's name.
#[cfg(unix)]
#[test]
fn validate_package_through_a_link_uses_the_links_name() {
    let home = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    let build = source.path().join("coding-release");
    write(
        &build,
        "agents/a.md",
        "---\ntitle_agent: coding/b\n---\nhi\n",
    );
    write(&build, "agents/b.md", AGENT);
    let link = source.path().join("coding");
    std::os::unix::fs::symlink(&build, &link).unwrap();

    let output = validate(home.path(), &["--package", link.to_str().unwrap()]);

    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
}
