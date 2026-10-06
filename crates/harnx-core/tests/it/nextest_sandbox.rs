//! Black-box tests for `scripts/nextest-sandbox`, the Cargo target runner that
//! gives every nextest test a private environment (AGENTS.md, "Test sandbox").
//! Each test runs the script directly with an environment it controls, which
//! stands in for the shell nextest was started from.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// A stand-in for the shell nextest is started from.
struct Shell {
    vars: BTreeMap<String, String>,
}

impl Shell {
    /// nextest running a test, with this test's own `PATH` (so `bwrap`, `env`
    /// and `sh` resolve) and `home` as `HOME`.
    fn test_run(home: &Path) -> Self {
        let path = std::env::var("PATH").expect("PATH is set");
        Self {
            vars: BTreeMap::new(),
        }
        .set("PATH", &path)
        .set("HOME", home.to_str().expect("UTF-8 home"))
        .set("NEXTEST", "1")
        .set("NEXTEST_ATTEMPT_ID", "nextest-sandbox-test")
    }

    fn set(mut self, name: &str, value: &str) -> Self {
        self.vars.insert(name.to_string(), value.to_string());
        self
    }

    fn remove(mut self, name: &str) -> Self {
        self.vars.remove(name);
        self
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(bash());
        command
            .arg(runner())
            .args(args)
            .env_clear()
            .envs(&self.vars);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args)
            .output()
            .expect("run scripts/nextest-sandbox")
    }
}

fn runner() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/nextest-sandbox")
        .canonicalize()
        .expect("scripts/nextest-sandbox exists")
}

/// Found up front, so a test can run the script with an empty `PATH`.
fn bash() -> PathBuf {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join("bash"))
        .find(|candidate| candidate.is_file())
        .expect("bash is on PATH")
}

/// Outside `/tmp`, which the sandbox replaces on Linux.
fn fake_home() -> tempfile::TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("create a fake home")
}

/// Under `/tmp`, which the sandbox replaces on Linux. When these tests run
/// sandboxed themselves, the outer sandbox's `/tmp` stands in for the host's.
fn tmp_dir() -> tempfile::TempDir {
    tempfile::tempdir_in("/tmp").expect("create a directory under /tmp")
}

fn stdout(output: &Output) -> String {
    assert!(
        output.status.success(),
        "runner failed with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn env_of(output: &Output) -> BTreeMap<String, String> {
    stdout(output)
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

/// A word unique to this test, passed to the sandboxed command as `$0` so
/// `pgrep -f` can tell whether anything carrying it is still running.
struct Marker(String);

impl Marker {
    /// The counter keeps markers apart when tests share a process, as they do
    /// under `cargo test`. `pgrep -f` matches substrings, so the numbers sit
    /// between fixed words: at the end, test 1234's marker would also match
    /// test 12345's when tests share a PID namespace (unsandboxed runs, macOS).
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let call = NEXT.fetch_add(1, Ordering::Relaxed);
        Self(format!(
            "nextest-sandbox-{}-{call}-marker",
            std::process::id()
        ))
    }

    fn as_arg(&self) -> &str {
        &self.0
    }

    /// pgrep exits 1 when nothing matches; any other failure is an error, not
    /// proof that nothing is running.
    fn is_running(&self) -> bool {
        let status = Command::new("pgrep")
            .arg("-f")
            .arg(&self.0)
            .stdout(Stdio::null())
            .status()
            .expect("run pgrep");
        match status.code() {
            Some(0) => true,
            Some(1) => false,
            _ => panic!("pgrep failed: {status}"),
        }
    }
}

fn wait_until(condition: impl Fn() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn passes_through_when_nextest_is_not_running_a_test() {
    let home = fake_home();
    let output = Shell::test_run(home.path())
        .remove("NEXTEST_ATTEMPT_ID")
        .set("GEMINI_API_KEY", "developer-key")
        .run(&["env"]);
    let env = env_of(&output);
    assert_eq!(
        env.get("GEMINI_API_KEY").map(String::as_str),
        Some("developer-key")
    );
    assert!(!env.contains_key("XDG_CONFIG_HOME"), "sandboxed anyway");
}

#[test]
fn passes_through_when_switched_off() {
    let home = fake_home();
    let output = Shell::test_run(home.path())
        .set("HARNX_TEST_SANDBOX", "off")
        .set("GEMINI_API_KEY", "developer-key")
        .run(&["env"]);
    let env = env_of(&output);
    assert_eq!(
        env.get("GEMINI_API_KEY").map(String::as_str),
        Some("developer-key")
    );
}

#[test]
fn rejects_an_unknown_switch_value() {
    let home = fake_home();
    let output = Shell::test_run(home.path())
        .set("HARNX_TEST_SANDBOX", "of")
        .run(&["env"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains("HARNX_TEST_SANDBOX must be unset or 'off'"),
        "{stderr}"
    );
}

#[test]
fn passes_only_allowed_variables() {
    const KEPT: &[(&str, &str)] = &[
        ("LANG", "C.UTF-8"),
        ("CI", "true"),
        ("NATS_SERVER_BIN", "/opt/nats/nats-server"),
        ("CARGO_PKG_NAME", "demo"),
        (
            "CARGO_BIN_EXE_harnx-bash-tools",
            "/opt/bin/harnx-bash-tools",
        ),
        ("NEXTEST_TEST_NAME", "demo::passes"),
        ("RUSTFLAGS", "-C link-arg=-Wl,--as-needed --cfg x=\"y z\""),
        ("INSTA_UPDATE", "always"),
    ];
    const DROPPED: &[(&str, &str)] = &[
        ("GEMINI_API_KEY", "developer-key"),
        ("CARGO_REGISTRY_TOKEN", "registry-token"),
        ("SSH_AUTH_SOCK", "/run/user/1000/ssh-agent.sock"),
        ("GIT_DIR", "/home/dev/repo/.git"),
        ("HARNX_NATS_URL", "nats://127.0.0.1:4222"),
        ("HTTPS_PROXY", "http://proxy.local:3128"),
        ("TMUX", "/tmp/tmux-1000/default,1234,0"),
        ("XDG_CONFIG_HOME", "/home/dev/.config"),
    ];
    let home = fake_home();
    let shell = KEPT
        .iter()
        .chain(DROPPED)
        .fold(Shell::test_run(home.path()), |shell, (name, value)| {
            shell.set(name, value)
        });
    let env = env_of(&shell.run(&["env"]));
    let value = |name: &str| env.get(name).map(String::as_str);
    let altered: Vec<_> = KEPT
        .iter()
        .filter(|(name, expected)| value(name) != Some(*expected))
        .collect();
    let leaked: Vec<_> = DROPPED
        .iter()
        .filter(|(name, ambient)| value(name) == Some(*ambient))
        .collect();
    assert!(altered.is_empty(), "not passed through intact: {altered:?}");
    assert!(leaked.is_empty(), "reached the test: {leaked:?}");
}

/// Cargo's values can span lines (`CARGO_PKG_DESCRIPTION`). Without
/// `/proc`, as on macOS, the runner reads each value back on its own, where
/// trailing newlines are easy to lose.
#[test]
fn passes_multi_line_values_through_intact() {
    let home = fake_home();
    let description = "A demo crate.\nSecond line.\n\n";
    let printed = stdout(
        &Shell::test_run(home.path())
            .set("CARGO_PKG_DESCRIPTION", description)
            .run(&["printenv", "CARGO_PKG_DESCRIPTION"]),
    );
    assert_eq!(printed, format!("{description}\n"));
}

#[test]
fn passes_the_test_status_through() {
    let home = fake_home();
    let output = Shell::test_run(home.path()).run(&["sh", "-c", "exit 7"]);
    assert_eq!(
        output.status.code(),
        Some(7),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn gives_the_test_a_private_home() {
    let home = fake_home();
    let env = env_of(&Shell::test_run(home.path()).run(&["env"]));
    let dir =
        |name: &str| PathBuf::from(env.get(name).unwrap_or_else(|| panic!("{name} is unset")));
    let private = dir("XDG_CONFIG_HOME")
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
    .filter(|name| dir(name).parent() != Some(private.as_path()))
    .collect();
    let underived: Vec<&str> = [
        ("HARNX_CONFIG_DIR", "XDG_CONFIG_HOME"),
        ("HARNX_DATA_DIR", "XDG_DATA_HOME"),
        ("HARNX_STATE_DIR", "XDG_STATE_HOME"),
    ]
    .into_iter()
    .filter(|(harnx, xdg)| dir(harnx) != dir(xdg).join("harnx"))
    .map(|(harnx, _)| harnx)
    .collect();
    assert!(misplaced.is_empty(), "outside {private:?}: {misplaced:?}");
    assert!(
        underived.is_empty(),
        "not under their XDG directory: {underived:?}"
    );
    assert!(
        !private.starts_with(home.path()),
        "{private:?} is inside HOME"
    );
}

/// On macOS the private home goes under `/tmp`, which is a symlink. The runner
/// finds leftover processes by the home's path, so the path the test is given
/// must already be resolved.
#[test]
fn names_the_private_home_by_its_resolved_path() {
    let home = fake_home();
    let printed = stdout(&Shell::test_run(home.path()).run(&[
        "sh",
        "-c",
        "echo \"$XDG_CONFIG_HOME\"; cd \"$XDG_CONFIG_HOME\" && pwd -P",
    ]));
    let lines: Vec<&str> = printed.lines().collect();
    assert_eq!(lines.len(), 2, "{printed}");
    assert_eq!(lines[0], lines[1], "given vs resolved");
}

/// Tests create Unix sockets under TMPDIR, and macOS caps a socket path at
/// 104 bytes, so the runner keeps TMPDIR short even when the ambient one, like
/// macOS's per-user directory, is long. 40 bytes leaves 64 for the rest.
#[test]
fn keeps_tmpdir_short_enough_for_unix_sockets() {
    let home = fake_home();
    let long = home.path().join("a-long-ambient-temporary-directory-name");
    std::fs::create_dir(&long).expect("create the ambient TMPDIR");
    let printed = stdout(
        &Shell::test_run(home.path())
            .set("TMPDIR", long.to_str().expect("UTF-8 path"))
            .run(&["sh", "-c", "echo \"$TMPDIR\""]),
    );
    let tmpdir = printed.trim();
    assert!(
        tmpdir.len() <= 40,
        "TMPDIR is {} bytes: {tmpdir}",
        tmpdir.len()
    );
}

#[test]
fn keeps_the_runtime_directory_private_to_the_user() {
    let home = fake_home();
    let listing =
        stdout(&Shell::test_run(home.path()).run(&["sh", "-c", "ls -ld \"$XDG_RUNTIME_DIR\""]));
    assert!(listing.starts_with("drwx------"), "{listing}");
}

#[test]
fn passes_the_command_and_its_arguments_through_unchanged() {
    let home = fake_home();
    let printed =
        stdout(&Shell::test_run(home.path()).run(&["printf", "[%s]\\n", "two words", "", "a=b"]));
    assert_eq!(printed, "[two words]\n[]\n[a=b]\n");
}

/// Agent worktrees, for one, can live under `/tmp`.
#[test]
fn runs_in_a_directory_under_tmp() {
    let home = fake_home();
    let dir = tmp_dir();
    std::fs::write(dir.path().join("checkout-file"), "").expect("write a file in the directory");
    let output = Shell::test_run(home.path())
        .command(&["ls", "-A"])
        .current_dir(dir.path())
        .output()
        .expect("run scripts/nextest-sandbox");
    assert_eq!(stdout(&output), "checkout-file\n");
}

/// A target directory under `/tmp` puts the test binary there.
#[test]
fn runs_a_command_under_tmp() {
    use std::os::unix::fs::PermissionsExt;
    let home = fake_home();
    let dir = tmp_dir();
    let command = dir.path().join("test-binary");
    std::fs::write(&command, "#!/bin/sh\necho ran\n").expect("write the command");
    std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o755))
        .expect("make the command executable");
    let output = Shell::test_run(home.path()).run(&[command.to_str().expect("UTF-8 path")]);
    assert_eq!(stdout(&output), "ran\n");
}

/// `cargo insta test` has every test append the snapshots it used to one file
/// under `/tmp`, and treats a snapshot missing from it as unreferenced.
#[test]
fn lets_tests_append_to_insta_files_under_tmp() {
    let home = fake_home();
    let dir = tmp_dir();
    let references = dir.path().join("snapshot-references");
    let shell = Shell::test_run(home.path()).set(
        "INSTA_SNAPSHOT_REFERENCES_FILE",
        references.to_str().expect("UTF-8 path"),
    );
    for snapshot in ["first.snap", "second.snap"] {
        stdout(&shell.run(&[
            "sh",
            "-c",
            "echo \"$0\" >> \"$INSTA_SNAPSHOT_REFERENCES_FILE\"",
            snapshot,
        ]));
    }
    let recorded = std::fs::read_to_string(&references).expect("read the references file");
    assert_eq!(recorded, "first.snap\nsecond.snap\n");
}

/// Containers sometimes put HOME under `/tmp`.
#[test]
fn sees_a_home_under_tmp() {
    let home = tmp_dir();
    std::fs::write(home.path().join("home-file"), "").expect("write a file in HOME");
    let output = Shell::test_run(home.path()).run(&["sh", "-c", "ls -A \"$HOME\""]);
    assert_eq!(stdout(&output), "home-file\n");
}

#[test]
fn does_not_create_harnx_directories_on_the_host() {
    let home = fake_home();
    stdout(&Shell::test_run(home.path()).run(&[
        "sh",
        "-c",
        "mkdir -p \"$HARNX_DATA_DIR\" && touch \"$HARNX_DATA_DIR/file\"",
    ]));
    let created: Vec<PathBuf> = [".config/harnx", ".local/share/harnx", ".local/state/harnx"]
        .into_iter()
        .map(|dir| home.path().join(dir))
        .filter(|dir| dir.exists())
        .collect();
    assert!(created.is_empty(), "created on the host: {created:?}");
}

#[test]
fn stops_the_test_when_nextest_terminates_the_runner() {
    let home = fake_home();
    let marker = Marker::new();
    // `; exit 0` stops sh from exec'ing sleep, so the marker stays visible.
    let mut child = Shell::test_run(home.path())
        .command(&[
            "sh",
            "-c",
            "touch \"$HOME/ready\"; sleep 300; exit 0",
            marker.as_arg(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start the runner");
    wait_until(|| home.path().join("ready").exists(), "the test to start");
    wait_until(|| marker.is_running(), "pgrep to see the test");
    let killed = Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status()
        .expect("run kill");
    assert!(killed.success(), "kill failed");
    child.wait().expect("the runner exits");
    wait_until(|| !marker.is_running(), "the test to stop");
}

/// Puts a file in each of `dirs`, which stand for the developer's harnx
/// directories, and checks that the sandboxed command sees none of them.
#[cfg(target_os = "linux")]
fn assert_hidden(shell: &Shell, dirs: &[PathBuf]) {
    for dir in dirs {
        std::fs::create_dir_all(dir).expect("create a developer directory");
        std::fs::write(dir.join("developer-file"), "").expect("write a developer file");
    }
    let output = shell
        .command(&["ls", "-A"])
        .args(dirs)
        .output()
        .expect("run scripts/nextest-sandbox");
    let listing = stdout(&output);
    assert!(!listing.contains("developer-file"), "{listing}");
}

/// Each kind of directory is found a different way here: config at HOME's
/// default, data under `XDG_DATA_HOME` and state through the override.
#[cfg(target_os = "linux")]
#[test]
fn hides_the_developers_harnx_directories() {
    let home = fake_home();
    let custom = fake_home();
    let state = custom.path().join("harnx-state");
    let shell = Shell::test_run(home.path())
        .set(
            "XDG_DATA_HOME",
            custom.path().join("xdg-data").to_str().expect("UTF-8 path"),
        )
        .set("HARNX_STATE_DIR", state.to_str().expect("UTF-8 path"));
    assert_hidden(
        &shell,
        &[
            home.path().join(".config/harnx"),
            custom.path().join("xdg-data/harnx"),
            state,
        ],
    );
}

/// harnx uses the first of these that is set, but the others can still hold
/// the developer's files.
#[cfg(target_os = "linux")]
#[test]
fn hides_every_place_a_harnx_directory_can_be() {
    let home = fake_home();
    let custom = fake_home();
    let data = custom.path().join("harnx-data");
    let shell = Shell::test_run(home.path())
        .set("HARNX_DATA_DIR", data.to_str().expect("UTF-8 path"))
        .set(
            "XDG_DATA_HOME",
            custom.path().join("xdg-data").to_str().expect("UTF-8 path"),
        );
    assert_hidden(
        &shell,
        &[
            data,
            custom.path().join("xdg-data/harnx"),
            home.path().join(".local/share/harnx"),
        ],
    );
}

#[cfg(target_os = "linux")]
#[test]
fn hides_developer_directories_reached_through_a_symlink() {
    let home = fake_home();
    let real_dir = fake_home();
    let real_path = real_dir.path().to_path_buf();
    std::fs::write(real_path.join("developer-file"), "")
        .expect("write a developer file in real directory");
    let symlink_path = home.path().join(".config/harnx");
    std::fs::create_dir_all(home.path().join(".config")).expect("create .config");
    std::os::unix::fs::symlink(&real_path, &symlink_path).expect("create absolute symlink");
    let symlink_str = symlink_path.to_str().expect("UTF-8 path");
    let listing = stdout(&Shell::test_run(home.path()).run(&["ls", "-A", symlink_str]));
    assert!(
        !listing.contains("developer-file"),
        "developer-file visible through symlink: {listing}"
    );
}

/// The sandbox binds the `/tmp` entry that holds HOME back in, so the private
/// `/tmp` doesn't hide a harnx directory inside it and the directory needs a
/// mask of its own. The other mask tests keep HOME under the target
/// directory, which is outside `/tmp` in CI, so only this one covers that.
#[cfg(target_os = "linux")]
#[test]
fn hides_a_harnx_directory_inside_a_home_under_tmp() {
    let home = tmp_dir();
    assert_hidden(
        &Shell::test_run(home.path()),
        &[home.path().join(".local/share/harnx")],
    );
}

#[cfg(target_os = "linux")]
#[test]
fn gives_the_test_its_own_network_and_processes() {
    let home = fake_home();
    let seen = stdout(&Shell::test_run(home.path()).run(&[
        "sh",
        "-c",
        "cat /proc/1/comm; tail -n +3 /proc/net/dev | cut -d: -f1",
    ]));
    let seen: Vec<&str> = seen.lines().map(str::trim).collect();
    assert_eq!(seen, ["bwrap", "lo"]);
}

#[cfg(target_os = "linux")]
#[test]
fn kills_what_the_test_leaves_behind() {
    let home = fake_home();
    let marker = Marker::new();
    stdout(&Shell::test_run(home.path()).run(&[
        "sh",
        "-c",
        "sh -c 'sleep 300; exit 0' \"$0\" >/dev/null 2>&1 & exit 0",
        marker.as_arg(),
    ]));
    wait_until(|| !marker.is_running(), "the leftover process to die");
}

#[cfg(target_os = "linux")]
#[test]
fn requires_bubblewrap() {
    let home = fake_home();
    let empty = tempfile::tempdir().expect("create an empty PATH directory");
    let output = Shell::test_run(home.path())
        .set("PATH", empty.path().to_str().expect("UTF-8 path"))
        .run(&["/usr/bin/env"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(127), "{stderr}");
    assert!(
        stderr.contains("bubblewrap is required on Linux"),
        "{stderr}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn removes_the_private_home_and_what_the_test_leaves_behind() {
    let home = fake_home();
    let marker = Marker::new();
    let printed = stdout(&Shell::test_run(home.path()).run(&[
        "sh",
        "-c",
        "echo \"$XDG_CONFIG_HOME\"; sh -c 'sleep 300; exit 0' \"$0\" >/dev/null 2>&1 & exit 0",
        marker.as_arg(),
    ]));
    let private = PathBuf::from(printed.trim())
        .parent()
        .expect("private home")
        .to_path_buf();
    assert!(!private.exists(), "{private:?} was left behind");
    wait_until(|| !marker.is_running(), "the leftover process to die");
}
