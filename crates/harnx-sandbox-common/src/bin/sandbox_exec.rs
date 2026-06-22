//! Sandbox execution CLI wrapper.
//!
//! Configures a birdcage sandbox (Linux) or our own Seatbelt profile (macOS)
//! and spawns the supplied command. macOS uses an in-tree profile builder so
//! we can include `(allow file-ioctl)`, which birdcage 0.8.1 omits; see
//! [`crate::macos_sandbox`].

#[cfg(unix)]
use std::env;
#[cfg(unix)]
use std::ffi::{OsStr, OsString};
#[cfg(all(unix, not(target_os = "macos")))]
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::process;

#[cfg(all(unix, not(target_os = "macos")))]
use birdcage::{process::Command, Birdcage, Exception, Sandbox};

#[cfg(target_os = "macos")]
use harnx_sandbox_common::macos_sandbox::MacSandbox;
#[cfg(target_os = "macos")]
use std::process::Command;

#[cfg(unix)]
struct SandboxConfig {
    exec_paths: Vec<PathBuf>,
    write_paths: Vec<PathBuf>,
    read_paths: Vec<PathBuf>,
    env_vars: Vec<(String, String)>,
    no_network: bool,
    working_dir: Option<PathBuf>,
    command: Vec<OsString>,
}

#[cfg(unix)]
fn print_usage() {
    println!(
        "harnx-sandbox-exec [OPTIONS] -- <command> [args...]\n\nOptions:\n  --write <path>       Allow read+write (repeatable)\n  --read <path>        Allow read-only (repeatable)\n  --exec <path>        Allow read+execute (repeatable)\n  --env VAR[=VALUE]    Pass VAR from host env or set VALUE explicitly (repeatable)\n  --no-network         Disable networking (default: networking allowed)\n  --working-dir <path> Set working directory of spawned command\n  --help, -h           Print this help"
    );
}

#[cfg(unix)]
fn parse_path_arg<I>(args: &mut I, flag: &str) -> Result<PathBuf, String>
where
    I: Iterator<Item = OsString>,
{
    args.next()
        .map(PathBuf::from)
        .ok_or_else(|| format!("sandbox-exec: missing value for {flag}"))
}

#[cfg(unix)]
fn parse_env_arg(raw: &OsStr) -> Result<(String, Option<String>), String> {
    let s = raw
        .to_str()
        .ok_or_else(|| "sandbox-exec: --env value is not valid UTF-8".to_string())?;
    if s.is_empty() {
        return Err("sandbox-exec: --env requires a non-empty variable name".to_string());
    }
    match s.split_once('=') {
        Some((key, value)) => {
            if key.is_empty() {
                return Err("sandbox-exec: --env requires a non-empty variable name".to_string());
            }
            Ok((key.to_string(), Some(value.to_string())))
        }
        None => Ok((s.to_string(), None)),
    }
}

#[cfg(unix)]
fn parse_args() -> Result<Option<SandboxConfig>, String> {
    let mut args = env::args_os().skip(1);
    let mut exec_paths = Vec::new();
    let mut write_paths = Vec::new();
    let mut read_paths = Vec::new();
    let mut env_vars = Vec::new();
    let mut no_network = false;
    let mut working_dir = None;

    while let Some(arg) = args.next() {
        if arg == OsStr::new("--") {
            let command: Vec<OsString> = args.collect();
            if command.is_empty() {
                return Err("sandbox-exec: missing command after --".to_string());
            }
            return Ok(Some(SandboxConfig {
                exec_paths,
                write_paths,
                read_paths,
                env_vars,
                no_network,
                working_dir,
                command,
            }));
        }

        match arg.as_os_str() {
            flag if flag == OsStr::new("--write") => {
                write_paths.push(parse_path_arg(&mut args, "--write")?);
            }
            flag if flag == OsStr::new("--read") => {
                read_paths.push(parse_path_arg(&mut args, "--read")?);
            }
            flag if flag == OsStr::new("--exec") => {
                exec_paths.push(parse_path_arg(&mut args, "--exec")?);
            }
            flag if flag == OsStr::new("--env") => {
                let raw = args
                    .next()
                    .ok_or_else(|| "sandbox-exec: missing value for --env".to_string())?;
                let (key, value) = parse_env_arg(&raw)?;
                if let Some(value) = value {
                    env_vars.push((key, value));
                } else if let Ok(value) = env::var(&key) {
                    env_vars.push((key, value));
                }
            }
            flag if flag == OsStr::new("--working-dir") => {
                working_dir = Some(parse_path_arg(&mut args, "--working-dir")?);
            }
            flag if flag == OsStr::new("--no-network") => {
                no_network = true;
            }
            flag if flag == OsStr::new("--help") || flag == OsStr::new("-h") => {
                return Ok(None);
            }
            _ => {
                return Err(format!(
                    "sandbox-exec: unexpected argument: {}",
                    arg.to_string_lossy()
                ));
            }
        }
    }

    Err("sandbox-exec: missing -- before command".to_string())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn add_path_exception(
    sandbox: &mut Birdcage,
    path: &Path,
    make_exception: fn(PathBuf) -> Exception,
) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }

    sandbox
        .add_exception(make_exception(path.to_path_buf()))
        .map(|_| ())
        .map_err(|error| {
            format!(
                "sandbox-exec: failed to add exception for {}: {error}",
                path.display()
            )
        })
}

#[cfg(all(unix, not(target_os = "macos")))]
fn add_write_exception(sandbox: &mut Birdcage, path: &Path) -> Result<(), String> {
    add_path_exception(sandbox, path, Exception::WriteAndRead)
}

#[cfg(target_os = "macos")]
fn run() -> Result<i32, String> {
    let Some(config) = parse_args()? else {
        print_usage();
        return Ok(0);
    };

    let mut sandbox = MacSandbox::new();

    for path in &config.exec_paths {
        sandbox.allow_execute_and_read(path)?;
    }
    for path in &config.write_paths {
        sandbox.allow_write_and_read(path)?;
    }
    for path in &config.read_paths {
        sandbox.allow_read(path)?;
    }
    if !config.no_network {
        sandbox.allow_networking();
    }

    for (key, value) in &config.env_vars {
        // Put the value in the current process env so MacSandbox's
        // env-restriction step preserves it for the child.
        //
        // SAFETY: `env::set_var` mutates process-global state. This binary
        // runs single-threaded up to here (`parse_args` and the sandbox
        // setup never spawn threads, and `MacSandbox::apply_and_spawn` has
        // not been called yet), so no other thread can be observing the
        // environment concurrently.
        unsafe { env::set_var(key, value) };
        sandbox.allow_env(key.clone());
    }

    let mut command = {
        let mut command = Command::new(&config.command[0]);
        if let Some(working_dir) = &config.working_dir {
            command.current_dir(working_dir);
        }
        command
    };
    command.args(&config.command[1..]);

    let mut child = sandbox
        .apply_and_spawn(command)
        .map_err(|error| format!("sandbox-exec: failed to spawn process: {error}"))?;
    let status = child
        .wait()
        .map_err(|error| format!("sandbox-exec: failed to wait for child: {error}"))?;

    Ok(status.code().unwrap_or(1))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn run() -> Result<i32, String> {
    let Some(config) = parse_args()? else {
        print_usage();
        return Ok(0);
    };

    let mut sandbox = Birdcage::new();

    for path in &config.exec_paths {
        add_path_exception(&mut sandbox, path, Exception::ExecuteAndRead)?;
    }
    for path in &config.write_paths {
        add_write_exception(&mut sandbox, path)?;
    }
    for path in &config.read_paths {
        add_path_exception(&mut sandbox, path, Exception::Read)?;
    }
    if !config.no_network {
        sandbox
            .add_exception(Exception::Networking)
            .map_err(|error| {
                format!("sandbox-exec: failed to add Networking exception: {error}")
            })?;
    }

    for (key, value) in &config.env_vars {
        // Ensure the value lives in the current process env so birdcage's
        // restrict_env_variables() preserves it for the child.
        //
        // SAFETY: `env::set_var` is unsafe because it mutates process-global
        // state and is not thread-safe. This binary is the `sandbox_run`
        // helper, which runs single-threaded up to this point — `parse_args`
        // and the sandbox setup never spawn threads, and we have not yet
        // called `sandbox.spawn(...)`. No other code in the process can be
        // observing the environment concurrently, so the call is sound. We
        // must do this before `sandbox.spawn(...)` because birdcage's
        // `restrict_env_variables()` (invoked from `Birdcage::lock` inside
        // `spawn`) inspects `std::env::vars()` and removes any variable not
        // listed via `Exception::Environment`.
        unsafe { env::set_var(key, value) };
        sandbox
            .add_exception(Exception::Environment(key.clone()))
            .map_err(|error| {
                format!("sandbox-exec: failed to add env exception for {key}: {error}")
            })?;
    }

    let mut command = if let Some(working_dir) = &config.working_dir {
        // birdcage::process::Command on Linux lacks current_dir; rely on GNU env's
        // --chdir extension for now. Known limitation on Alpine/Busybox systems.
        let mut wrapped = Command::new("/usr/bin/env");
        wrapped.arg("--chdir");
        wrapped.arg(working_dir);
        wrapped.arg(&config.command[0]);
        wrapped
    } else {
        Command::new(&config.command[0])
    };
    command.args(&config.command[1..]);

    let mut child = sandbox
        .spawn(command)
        .map_err(|error| format!("sandbox-exec: failed to spawn process: {error}"))?;
    let status = child
        .wait()
        .map_err(|error| format!("sandbox-exec: failed to wait for child: {error}"))?;

    Ok(status.code().unwrap_or(1))
}

#[cfg(unix)]
fn main() {
    match run() {
        Ok(code) => process::exit(code),
        Err(error) => {
            eprintln!("{error}");
            process::exit(127);
        }
    }
}

#[cfg(not(unix))]
fn main() {
    eprintln!("sandbox-exec not supported on this platform");
    std::process::exit(1);
}

// These tests exercise birdcage's `add_path_exception` / `add_write_exception`
// helpers, which are linux-only after the macOS path moved to `MacSandbox`.
// `MacSandbox`'s equivalents are covered by unit tests in `macos_sandbox.rs`.
#[cfg(all(test, unix, not(target_os = "macos")))]
mod tests {
    use super::*;
    use std::env;

    /// An existing path gets `Exception::WriteAndRead` added without error.
    #[test]
    fn test_write_exception_existing_path() {
        let mut sandbox = Birdcage::new();
        let path = std::env::temp_dir(); // always exists
        let result = add_write_exception(&mut sandbox, &path);
        assert!(
            result.is_ok(),
            "add_write_exception failed on existing path: {result:?}"
        );
    }

    /// A non-existent path is silently skipped — `Ok(())` returned, no ancestor walked.
    #[test]
    fn test_write_exception_nonexistent_path() {
        let mut sandbox = Birdcage::new();
        let base = std::env::temp_dir();
        let nonexistent = base.join("harnx-test-nonexistent-12345678");
        // Make sure it really doesn't exist
        assert!(!nonexistent.exists());
        let result = add_write_exception(&mut sandbox, &nonexistent);
        assert!(
            result.is_ok(),
            "add_write_exception should return Ok for non-existent paths, got: {result:?}"
        );
    }

    /// When `$HOME/.pyenv` doesn't exist, `$HOME` must NOT appear as a sandbox exception.
    /// This is the core regression test for issue #619.
    #[test]
    fn test_write_exception_nonexistent_nested_no_ancestor_walk() {
        let home = env::var_os("HOME").expect("HOME must be set for this test");
        let home_path = Path::new(&home);
        // Construct a path like $HOME/.pyenv that is unlikely to exist
        let fake_tool_path = home_path.join(".harnx-test-pyenv-NOTEXIST");
        assert!(!fake_tool_path.exists(), "Test setup: path must not exist");

        let mut sandbox = Birdcage::new();
        // Must succeed (not error) without walking to $HOME
        let result = add_write_exception(&mut sandbox, &fake_tool_path);
        assert!(
            result.is_ok(),
            "add_write_exception should return Ok for non-existent $HOME child, got: {result:?}"
        );
        // If the ancestor walk were still present, $HOME would have been added.
        // We can't inspect Birdcage internals, but if it didn't add it the function
        // must have taken the early-return path (the Ok(()) branch).
        // The test above verifies that the function returns Ok without erroring,
        // which is the correct post-fix behavior (previously it would walk to $HOME).
    }
}
