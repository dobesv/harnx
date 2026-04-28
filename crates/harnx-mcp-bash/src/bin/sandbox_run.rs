//! Sandbox execution CLI wrapper.
//!
//! Configures birdcage sandbox and spawns supplied command.

#[cfg(unix)]
use std::env;
#[cfg(unix)]
use std::ffi::{OsStr, OsString};
#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process;

#[cfg(unix)]
use birdcage::{process::Command, Birdcage, Exception, Sandbox};

#[cfg(unix)]
struct SandboxConfig {
    exec_paths: Vec<PathBuf>,
    write_paths: Vec<PathBuf>,
    read_paths: Vec<PathBuf>,
    no_network: bool,
    working_dir: Option<PathBuf>,
    command: Vec<OsString>,
}

#[cfg(unix)]
fn print_usage() {
    println!(
        "harnx-mcp-bash-sandbox-run [OPTIONS] -- <command> [args...]\n\nOptions:\n  --write <path>       Allow read+write (repeatable)\n  --read <path>        Allow read-only (repeatable)\n  --exec <path>        Allow read+execute (repeatable)\n  --no-network         Disable networking (default: networking allowed)\n  --working-dir <path> Set working directory of spawned command\n  --help, -h           Print this help"
    );
}

#[cfg(unix)]
fn parse_path_arg<I>(args: &mut I, flag: &str) -> Result<PathBuf, String>
where
    I: Iterator<Item = OsString>,
{
    args.next()
        .map(PathBuf::from)
        .ok_or_else(|| format!("sandbox-run: missing value for {flag}"))
}

#[cfg(unix)]
fn parse_args() -> Result<Option<SandboxConfig>, String> {
    let mut args = env::args_os().skip(1);
    let mut exec_paths = Vec::new();
    let mut write_paths = Vec::new();
    let mut read_paths = Vec::new();
    let mut no_network = false;
    let mut working_dir = None;

    while let Some(arg) = args.next() {
        if arg == OsStr::new("--") {
            let command: Vec<OsString> = args.collect();
            if command.is_empty() {
                return Err("sandbox-run: missing command after --".to_string());
            }
            return Ok(Some(SandboxConfig {
                exec_paths,
                write_paths,
                read_paths,
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
                    "sandbox-run: unexpected argument: {}",
                    arg.to_string_lossy()
                ));
            }
        }
    }

    Err("sandbox-run: missing -- before command".to_string())
}

#[cfg(unix)]
fn add_path_exception(
    sandbox: &mut Birdcage,
    path: &Path,
    make_exception: fn(PathBuf) -> Exception,
) -> Result<(), String> {
    if !path.exists() {
        eprintln!(
            "sandbox-run: skipping non-existent path: {}",
            path.display()
        );
        return Ok(());
    }

    sandbox
        .add_exception(make_exception(path.to_path_buf()))
        .map(|_| ())
        .map_err(|error| {
            format!(
                "sandbox-run: failed to add exception for {}: {error}",
                path.display()
            )
        })
}

#[cfg(unix)]
fn add_write_exception(sandbox: &mut Birdcage, path: &Path) -> Result<(), String> {
    let target = if path.exists() {
        path.to_path_buf()
    } else {
        let mut current = path.parent();
        loop {
            match current {
                Some(parent) if parent.exists() => break parent.to_path_buf(),
                Some(parent) => current = parent.parent(),
                None => {
                    eprintln!(
                        "sandbox-run: skipping write path with no existing ancestor: {}",
                        path.display()
                    );
                    return Ok(());
                }
            }
        }
    };

    sandbox
        .add_exception(Exception::WriteAndRead(target.clone()))
        .map(|_| ())
        .map_err(|error| {
            format!(
                "sandbox-run: failed to add write exception for {}: {error}",
                target.display()
            )
        })
}

#[cfg(unix)]
fn run() -> Result<i32, String> {
    let Some(config) = parse_args()? else {
        print_usage();
        return Ok(0);
    };

    let mut sandbox = Birdcage::new();
    sandbox
        .add_exception(Exception::FullEnvironment)
        .map_err(|error| {
            format!("sandbox-run: failed to add FullEnvironment exception: {error}")
        })?;

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
            .map_err(|error| format!("sandbox-run: failed to add Networking exception: {error}"))?;
    }

    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new(&config.command[0]);
        if let Some(working_dir) = &config.working_dir {
            command.current_dir(working_dir);
        }
        command
    };

    #[cfg(not(target_os = "macos"))]
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
        .map_err(|error| format!("sandbox-run: failed to spawn process: {error}"))?;
    let status = child
        .wait()
        .map_err(|error| format!("sandbox-run: failed to wait for child: {error}"))?;

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
    eprintln!("sandbox-run not supported on this platform");
    std::process::exit(1);
}
