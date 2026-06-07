//! End-to-end integration test for synthetic acli config generation.
//!
//! CI-safe: no real acli install, no real keyring, no network, no host config use.

#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use serde_json::Value;
use tempfile::tempdir;

const SYNTHETIC_TOKEN_BLOB: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA6OjowMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDBhNmM2MzI1MzM1NGQxODBiNjkzYWFjYmRkZjlmYjA2YzFkMGI2NmE0MmQ4Mzc1NmJjM2U5ZjM5ODg4MzRhMGZiM2EzYTRhMWY=";
const HOST_TOKEN: &str = "REAL_HOST_TOKEN_DO_NOT_LEAK";

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

    let fs_root = find_fs_root(home.path()).expect("find harnx-fs temp dir");
    assert!(
        fs_root.exists(),
        "fs root should exist: {}",
        fs_root.display()
    );

    let rendered = fs_root.join("acli/jira_config.yaml");
    let rendered_contents = std::fs::read_to_string(&rendered).expect("read synthetic config");
    let parsed: Value =
        serde_json::from_str(&rendered_contents).expect("synthetic config should be JSON");

    // The proxy must select the profile matching `current_profile`, NOT
    // `profiles[0]` (which is the unrelated `othercloud` tenant in the fixture).
    assert_eq!(parsed["current_profile"], "realcloud123:realacct456");
    assert_eq!(parsed["profiles"][0]["cloud_id"], "realcloud123");
    assert_eq!(parsed["profiles"][0]["site"], "mycompany.atlassian.net");
    assert_eq!(parsed["profiles"][0]["token"], SYNTHETIC_TOKEN_BLOB);
    assert!(
        !rendered_contents.contains(HOST_TOKEN),
        "synthetic config leaked host token: {rendered_contents}"
    );
    // The wrong (non-active) profile must not have been selected.
    assert!(
        !rendered_contents.contains("othercloud") && !rendered_contents.contains("OTHER_PROFILE"),
        "synthetic config used the wrong (non-active) profile: {rendered_contents}"
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
    let fs_root = find_fs_root(home.path()).expect("find harnx-fs temp dir");

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

/// Token the fake keyring command emits — stands in for the real
/// `secret-tool` / `security` lookup so the test exercises the `--load-exec`
/// self-sourcing path without a real keyring.
const KEYRING_TOKEN: &str = "TOKEN_FROM_FAKE_KEYRING";

/// Write an executable that emits [`KEYRING_TOKEN`] only when invoked with the
/// expected `jira:<current_profile>` lookup key, mirroring how the shipped
/// `--load-exec` command calls `secret-tool`. Returns its path.
fn write_credential_script(home: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = home.join("fake-secret.sh");
    // Match the active profile's key (realcloud123:realacct456). Any other
    // lookup exits non-zero, so the proxy must derive the right key.
    let script = format!(
        "#!/usr/bin/env sh\ncase \"$*\" in\n  *\"jira:realcloud123:realacct456\"*) printf '%s' '{KEYRING_TOKEN}' ;;\n  *) exit 1 ;;\nesac\n"
    );
    std::fs::write(&path, script).expect("write credential script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("chmod credential script");
    path
}

fn spawn_proxy(proxy_bin: &Path, home: &Path) -> Child {
    let cred_script = write_credential_script(home);
    // Mirror the shipped bash.yaml wiring: derive the active profile's keyring
    // key from `current_profile` and fetch the token via `--load-exec`
    // (here pointed at a fake keyring command), then select the profile whose
    // `<cloud_id>:<account_id>` equals `current_profile` (NOT `profiles[0]`).
    let load_exec = format!(
        "atlassian_token=p=$(sed -n \"s/^current_profile:[[:space:]]*\\\"\\?\\([^\\\"]*\\)\\\"\\?[[:space:]]*$/\\1/p\" ~/.config/acli/jira_config.yaml); test -n \"$p\" && {} \"jira:$p\"",
        cred_script.display()
    );
    Command::new(proxy_bin)
        .args([
            "--load-yaml",
            "acli_cfg=~/.config/acli/jira_config.yaml",
            "--load-exec",
            &load_exec,
            "--fs",
            r#"$acli_cfg.current_profile as $cp |
                (first($acli_cfg.profiles[]? | select("\(.cloud_id):\(.account_id)" == $cp))) as $p |
                if $p and $atlassian_token then
                  . + {
                    "acli/jira_config.yaml": ({
                      version: 1,
                      current_profile: $cp,
                      profiles: [{
                        site: $p.site,
                        cloud_id: $p.cloud_id,
                        account_id: $p.account_id,
                        auth_type: "api_token",
                        token: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA6OjowMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDBhNmM2MzI1MzM1NGQxODBiNjkzYWFjYmRkZjlmYjA2YzFkMGI2NmE0MmQ4Mzc1NmJjM2U5ZjM5ODg4MzRhMGZiM2EzYTRhMWY="
                      }]
                    } | tojson)
                  }
                end"#,
            "--env",
            r#"$acli_cfg.current_profile as $cp |
                (first($acli_cfg.profiles[]? | select("\(.cloud_id):\(.account_id)" == $cp))) as $p |
                if $p and $atlassian_token then
                  .ACLI_CONFIG_DIR = $temp_file_root
                end"#,
            "--hook",
            r#"$acli_cfg.current_profile as $cp |
                (first($acli_cfg.profiles[]? | select("\(.cloud_id):\(.account_id)" == $cp))) as $p |
                if $p and $atlassian_token and (.host == "api.atlassian.com" or .host == $p.site)
                then .headers.authorization = basic($p.email // env.ATLASSIAN_EMAIL // ""; $atlassian_token)
                end"#,
        ])
        .env("HOME", home)
        .env("TMPDIR", home)
        .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/tmp/nonexistent-harnx-test-bus")
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

fn find_fs_root(home: &Path) -> Option<PathBuf> {
    std::fs::read_dir(home)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("harnx-fs-"))
        })
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
