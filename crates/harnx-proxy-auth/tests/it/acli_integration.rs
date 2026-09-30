//! End-to-end integration test for resident Jira hook synthetic acli config generation.
//!
//! CI-safe: no real acli install, no real keyring, no network, no host config use.

#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::tempdir;

#[test]
fn acli_synthetic_config_written_from_host_yaml() {
    let Some(proxy_bin) = proxy_binary_path() else {
        eprintln!(
            "SKIP acli_synthetic_config_written_from_host_yaml: harnx-proxy-auth binary not found"
        );
        return;
    };

    let home = tempdir().expect("temp home");
    let config_dir = home.path().join(".config/acli");
    std::fs::create_dir_all(&config_dir).expect("create acli config dir");
    std::fs::write(config_dir.join("jira_config.yaml"), sample_host_config())
        .expect("write host yaml");

    let mut child = spawn_proxy(&proxy_bin, home.path());
    let (proxy_port, _ca_cert_path) = read_readiness(&mut child);
    assert!(proxy_port > 0, "proxy port should be non-zero");

    let rendered = find_rendered_config(home.path(), Duration::from_secs(5)).unwrap_or_else(|details| {
        panic!(
            "synthetic config should be written before readiness completes; search details:\n{details}"
        )
    });
    let rendered_contents = std::fs::read_to_string(&rendered).expect("read synthetic YAML config");

    // The proxy must select profile matching `current_profile`, NOT `profiles[0]`
    // (unrelated `othercloud` tenant in fixture), and keep sentinel token in plain scalar form.
    assert!(
        rendered_contents
            .starts_with("version: 1\ncurrent_profile: realcloud123:realacct456\nprofiles:\n"),
        "synthetic config should start with expected YAML header: {rendered_contents}"
    );
    assert!(
        rendered_contents.contains("current_profile: realcloud123:realacct456\n"),
        "rendered config should preserve current_profile: {rendered_contents}"
    );
    assert!(
        rendered_contents.contains("profiles:\n"),
        "rendered config should remain valid acli YAML: {rendered_contents}"
    );
    assert_eq!(
        rendered,
        std::fs::canonicalize(config_dir.join("jira_config.yaml"))
            .expect("canonicalize host config path"),
        "hook no longer rewrites host config path eagerly; test verifies env wiring instead"
    );

    let reported_dir =
        fetch_debug_acli_config_dir(proxy_port).expect("fetch debug ACLI_CONFIG_DIR");
    // Canonicalize both sides before comparing: on macOS `home` (a tempdir under
    // `/var/folders/...`) resolves through the `/var -> /private/var` symlink, so
    // the proxy-reported path is `/private/var/...` while the raw tempdir path is
    // `/var/...`. Compare real paths to stay portable.
    let expected_root = home.path().join("proxy-temp-root");
    std::fs::create_dir_all(&expected_root).expect("create expected proxy temp root");
    let expected_dir = std::fs::canonicalize(&expected_root)
        .expect("canonicalize expected proxy temp root")
        .join("harnx-fs-acli");
    let reported_dir = std::fs::canonicalize(reported_dir.parent().expect("reported dir parent"))
        .expect("canonicalize reported dir parent")
        .join(reported_dir.file_name().expect("reported dir file name"));
    assert_eq!(
        reported_dir, expected_dir,
        "sandbox ACLI_CONFIG_DIR should point at --env-injected synthetic config root"
    );

    kill_child(&mut child);
}

#[test]
#[ignore = "requires real acli binary, network, and real Atlassian credentials"]
fn acli_status_smoke_test_with_real_binary_and_creds() {
    let Some(proxy_bin) = proxy_binary_path() else {
        eprintln!("SKIP ignored smoke test: harnx-proxy-auth binary not found");
        return;
    };
    let Ok(acli_path) = which_acli() else {
        eprintln!("SKIP ignored smoke test: acli not on PATH");
        return;
    };
    if std::env::var("ATLASSIAN_EMAIL").is_err() || std::env::var("ATLASSIAN_API_TOKEN").is_err() {
        eprintln!("SKIP ignored smoke test: ATLASSIAN_EMAIL / ATLASSIAN_API_TOKEN not set");
        return;
    }

    let home = tempdir().expect("temp home");
    let config_dir = home.path().join(".config/acli");
    std::fs::create_dir_all(&config_dir).expect("create acli config dir");
    std::fs::write(config_dir.join("jira_config.yaml"), sample_host_config())
        .expect("write host yaml");

    let mut child = spawn_proxy(&proxy_bin, home.path());
    let (proxy_port, ca_cert_path) = read_readiness(&mut child);
    let rendered =
        find_rendered_config(home.path(), Duration::from_secs(5)).unwrap_or_else(|details| {
            panic!("find rendered acli config for smoke test; search details:\n{details}")
        });
    let fs_root = rendered
        .parent()
        .and_then(Path::parent)
        .expect("rendered config should live under <fs_root>/acli/jira_config.yaml")
        .to_path_buf();

    let status = Command::new(acli_path)
        .args(["jira", "auth", "status"])
        .env("HOME", home.path())
        .env("ACLI_CONFIG_DIR", &fs_root)
        .env("HTTPS_PROXY", format!("http://127.0.0.1:{proxy_port}"))
        .env("SSL_CERT_FILE", ca_cert_path)
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            "unix:path=/tmp/nonexistent-harnx-test-bus",
        )
        .status()
        .expect("run acli");

    kill_child(&mut child);
    assert!(status.success(), "acli status exit code: {status:?}");
}

/// Token the fake keyring command emits — stands in for real `secret-tool`
/// / `security` lookup so test exercises resident hook self-sourcing path
/// without real keyring.
const KEYRING_TOKEN: &str = "TOKEN_FROM_FAKE_KEYRING";

/// Write executable that emits [`KEYRING_TOKEN`] only when invoked with
/// expected `jira:<current_profile>` lookup key, mirroring shipped resident
/// hook's token command contract. Returns its path.
fn write_credential_script(home: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = home.join("fake-secret.sh");
    let script = format!(
        "#!/usr/bin/env sh\ncase \"$*\" in\n  *\"jira:realcloud123:realacct456\"*) printf '%s' '{KEYRING_TOKEN}' ;;\n  *) exit 1 ;;\nesac\n"
    );
    std::fs::write(&path, script).expect("write credential script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("chmod credential script");
    path
}

fn jira_hook_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("example_config")
        .join("jira-auth-hook.py")
}

fn spawn_proxy(proxy_bin: &Path, home: &Path) -> Child {
    let cred_script = write_credential_script(home);
    let jira_hook = jira_hook_path();
    let host_config = home.join(".config/acli/jira_config.yaml");
    let temp_root = home.join("proxy-temp-root");
    let sandbox_config_dir = temp_root.join("harnx-fs-acli");
    std::fs::create_dir_all(&temp_root).expect("create proxy temp root");
    std::fs::create_dir_all(&sandbox_config_dir).expect("create synthetic acli config dir");
    std::fs::copy(&host_config, sandbox_config_dir.join("jira_config.yaml"))
        .expect("seed synthetic acli config path");

    Command::new(proxy_bin)
        .args([
            "--env",
            r#"{"ACLI_CONFIG_DIR": "\($temp_file_root)/harnx-fs-acli"}"#,
            "--hook",
            &std::fs::read_to_string(&jira_hook).expect("read jira hook script"),
        ])
        .env("HOME", home)
        .env("TMPDIR", home)
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            "unix:path=/tmp/nonexistent-harnx-test-bus",
        )
        .env(
            "HARNX_JIRA_TOKEN_CMD",
            format!(
                r#"{} "jira:realcloud123:realacct456""#,
                cred_script.display()
            ),
        )
        .env("ACLI_HOST_CONFIG", &host_config)
        .env("HARNX_JIRA_HOST_CONFIG", &host_config)
        .env("HARNX_JIRA_TEMP_ROOT", &temp_root)
        .env("TEMP_FILE_ROOT", &temp_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn harnx-proxy-auth")
}

fn read_readiness(child: &mut Child) -> (u16, PathBuf) {
    let stdout = child.stdout.take().expect("stdout pipe");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    let mut proxy_port = None;
    let mut ca_cert_path = None;

    for _ in 0..10 {
        line.clear();
        let bytes = reader.read_line(&mut line).expect("read readiness line");
        assert!(bytes > 0, "proxy exited before readiness lines");
        let trimmed = line.trim();
        if let Some(value) = trimmed.strip_prefix("PROXY_PORT=") {
            proxy_port = Some(value.parse::<u16>().expect("parse proxy port"));
        }
        if let Some(value) = trimmed.strip_prefix("CA_CERT_PATH=") {
            ca_cert_path = Some(PathBuf::from(value));
        }
        if proxy_port.is_some() && ca_cert_path.is_some() {
            break;
        }
    }

    child.stdout = Some(reader.into_inner());
    (
        proxy_port.expect("PROXY_PORT readiness line"),
        ca_cert_path.expect("CA_CERT_PATH readiness line"),
    )
}

fn find_rendered_config(home: &Path, timeout: Duration) -> Result<PathBuf, String> {
    let roots = candidate_roots(home);
    let deadline = Instant::now() + timeout;
    let mut last_seen = Vec::new();

    while Instant::now() <= deadline {
        let mut seen_this_round = Vec::new();
        for root in &roots {
            find_rendered_config_under(root, &mut seen_this_round);
        }

        let mut non_empty = seen_this_round
            .iter()
            .filter(|path| file_is_non_empty(path))
            .collect::<Vec<_>>();
        non_empty.sort_by_key(|path| path.components().count());
        if let Some(found) = non_empty.into_iter().next() {
            return std::fs::canonicalize(found).map_err(|err| {
                format!(
                    "found rendered config at {} but failed to canonicalize: {err}",
                    found.display()
                )
            });
        }

        last_seen = seen_this_round;
        thread::sleep(Duration::from_millis(50));
    }

    Err(format_search_diagnostics(&roots, &last_seen))
}

fn fetch_debug_acli_config_dir(proxy_port: u16) -> Result<PathBuf, String> {
    let output = Command::new("curl")
        .args([
            "--silent",
            "--show-error",
            "--proxy",
            &format!("http://127.0.0.1:{proxy_port}"),
            "http://harnx.invalid/jira-auth-hook/debug",
        ])
        .output()
        .map_err(|err| format!("run curl for debug ACLI_CONFIG_DIR endpoint: {err}"))?;

    if !output.status.success() {
        return Err(format!(
            "curl debug endpoint failed: status={:?} stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let body: Value = serde_json::from_slice(&output.stdout)
        .map_err(|err| format!("parse debug ACLI_CONFIG_DIR JSON: {err}"))?;
    let dir = body
        .get("acli_config_dir")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("debug JSON missing acli_config_dir: {body}"))?;

    std::fs::canonicalize(dir)
        .map_err(|err| format!("canonicalize reported ACLI_CONFIG_DIR {dir}: {err}"))
}

fn candidate_roots(home: &Path) -> Vec<PathBuf> {
    let mut roots = vec![home.to_path_buf()];
    if let Ok(canonical_home) = std::fs::canonicalize(home) {
        if canonical_home != home {
            roots.push(canonical_home);
        }
    }
    roots
}

fn find_rendered_config_under(root: &Path, matches: &mut Vec<PathBuf>) {
    let candidate = root.join("acli/jira_config.yaml");
    if candidate.exists() {
        matches.push(candidate);
    }

    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };

    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            find_rendered_config_under(&path, matches);
        }
    }
}

fn file_is_non_empty(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.len() > 0)
        .unwrap_or(false)
}

fn format_search_diagnostics(roots: &[PathBuf], last_seen: &[PathBuf]) -> String {
    let mut details = String::new();
    if last_seen.is_empty() {
        details.push_str("No matching acli/jira_config.yaml files found.\n");
    } else {
        details.push_str("Matching paths seen but empty/unreadable:\n");
        for path in last_seen {
            details.push_str(&format!("- {}\n", path.display()));
        }
    }

    for root in roots {
        details.push_str(&format!("Tree under {}:\n", root.display()));
        append_tree(root, 0, &mut details);
    }
    details
}

fn append_tree(path: &Path, depth: usize, out: &mut String) {
    let indent = "  ".repeat(depth);
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            let kind = if meta.is_dir() {
                "/"
            } else if meta.file_type().is_symlink() {
                "@"
            } else {
                ""
            };
            out.push_str(&format!("{indent}{}{}\n", path.display(), kind));
            if meta.is_dir() {
                match std::fs::read_dir(path) {
                    Ok(entries) => {
                        let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
                        entries.sort_by_key(|entry| entry.path());
                        for entry in entries {
                            append_tree(&entry.path(), depth + 1, out);
                        }
                    }
                    Err(err) => {
                        out.push_str(&format!("{indent}  <read_dir error: {err}>\n"));
                    }
                }
            }
        }
        Err(err) => {
            out.push_str(&format!("{indent}{} <stat error: {err}>\n", path.display()));
        }
    }
}

fn kill_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn proxy_binary_path() -> Option<PathBuf> {
    if let Some(path) = std::option_env!("CARGO_BIN_EXE_harnx-proxy-auth") {
        let p = PathBuf::from(path);
        if p.is_file() {
            return Some(p);
        }
    }
    let candidate = target_dir().join(binary_name("harnx-proxy-auth"));
    candidate.is_file().then_some(candidate)
}

fn target_dir() -> PathBuf {
    let mut exe = std::env::current_exe().expect("current_exe");
    exe.pop();
    if exe.ends_with("deps") {
        exe.pop();
    }
    exe
}

fn binary_name(base: &str) -> String {
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base.to_string()
    }
}

fn which_acli() -> Result<PathBuf, std::io::Error> {
    let paths = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&paths) {
        let candidate = dir.join(binary_name("acli"));
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "acli not found on PATH",
    ))
}

fn sample_host_config() -> &'static str {
    // `current_profile` is `<cloud_id>:<account_id>` (acli's convention) and is
    // the SECOND profile, so the proxy must select by `current_profile` rather
    // than blindly using `profiles[0]`.
    r#"version: 1
current_profile: realcloud123:realacct456
profiles:
  - site: other.atlassian.net
    cloud_id: othercloud
    account_id: otheracct
    email: other@example.com
    auth_type: api_token
    token: OTHER_PROFILE_TOKEN
  - site: mycompany.atlassian.net
    cloud_id: realcloud123
    account_id: realacct456
    email: me@example.com
    auth_type: api_token
    token: REAL_HOST_TOKEN_DO_NOT_LEAK
"#
}
