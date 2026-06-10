use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, ExitStatus};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

fn main() {
    if let Err(err) = run() {
        eprintln!("Error: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    match parse_cli(env::args_os().skip(1).collect())? {
        CommandLine::Install(args) => install(args),
        CommandLine::Help => {
            print_help();
            Ok(())
        }
    }
}

enum CommandLine {
    Install(InstallArgs),
    Help,
}

struct InstallArgs {
    debug: bool,
    bins: Vec<String>,
}

fn parse_cli(args: Vec<OsString>) -> Result<CommandLine> {
    let Some(command) = args.first() else {
        return Ok(CommandLine::Help);
    };

    match command.to_str() {
        Some("install") => parse_install_args(&args[1..]).map(CommandLine::Install),
        Some("help") | Some("-h") | Some("--help") => Ok(CommandLine::Help),
        Some(other) => bail!("unknown subcommand `{other}`\n\n{}", help_text()),
        None => bail!("subcommand must be valid UTF-8"),
    }
}

fn parse_install_args(args: &[OsString]) -> Result<InstallArgs> {
    let mut debug = false;
    let mut bins = Vec::new();

    for arg in args {
        match arg.to_str() {
            Some("--debug") => debug = true,
            Some("-h") | Some("--help") => {
                print_install_help();
                std::process::exit(0);
            }
            Some(flag) if flag.starts_with('-') => {
                bail!("unknown install flag `{flag}`\n\n{}", install_help_text())
            }
            Some(bin) => bins.push(bin.to_owned()),
            None => bail!("bin name must be valid UTF-8"),
        }
    }

    Ok(InstallArgs { debug, bins })
}

fn install(args: InstallArgs) -> Result<()> {
    let metadata = cargo_metadata()?;
    let bins = discover_bins(&metadata)?;
    let selected_bins = select_bins(&bins, &args.bins)?;

    let profile_dir = if args.debug { "debug" } else { "release" };
    let mut build = Command::new("cargo");
    build.arg("build").arg("--locked");
    if !args.debug {
        build.arg("--release");
    }
    for bin in &selected_bins {
        build.arg("--bin").arg(bin);
    }

    run_command(&mut build, "cargo build")?;

    let target_dir = env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| metadata.target_directory.clone());
    let install_dir = cargo_install_root()?.join("bin");
    fs::create_dir_all(&install_dir)
        .with_context(|| format!("failed to create install dir {}", install_dir.display()))?;

    for bin in selected_bins {
        let executable_name = executable_name(&bin);
        let source = target_dir.join(profile_dir).join(&executable_name);
        let destination = install_dir.join(&executable_name);
        println!("==> Installing {bin} -> {}", destination.display());
        install_binary(&source, &destination)?;
    }

    Ok(())
}

/// Copy `source` over `destination` without failing when the destination is a
/// currently-running binary.
///
/// Writing directly to `destination` (as `fs::copy` does) fails with `ETXTBSY`
/// ("Text file busy") on Linux when the target is being executed, because the
/// open-for-truncate touches the in-use inode. Instead we copy to a sibling
/// temp file and atomically `rename` it over the destination: the rename swaps
/// the directory entry, leaving any running process pointing at the old, now
/// unlinked inode. This mirrors the `cp -f` behaviour the old Argcfile task
/// relied on, so `install` works without stopping existing harnx processes.
fn install_binary(source: &PathBuf, destination: &PathBuf) -> Result<()> {
    let file_name = destination.file_name().ok_or_else(|| {
        anyhow!(
            "install destination {} has no file name",
            destination.display()
        )
    })?;
    let mut tmp_name = OsString::from(".");
    tmp_name.push(file_name);
    tmp_name.push(".new");
    let tmp = destination.with_file_name(tmp_name);

    fs::copy(source, &tmp)
        .with_context(|| format!("failed to copy {} to {}", source.display(), tmp.display()))?;
    if let Err(err) = fs::rename(&tmp, destination) {
        let _ = fs::remove_file(&tmp);
        return Err(err).with_context(|| {
            format!(
                "failed to install {} to {}",
                tmp.display(),
                destination.display()
            )
        });
    }

    Ok(())
}

fn executable_name(bin: &str) -> String {
    format!("{bin}{}", env::consts::EXE_SUFFIX)
}

fn cargo_install_root() -> Result<PathBuf> {
    if let Some(path) = env::var_os("CARGO_INSTALL_ROOT") {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = env::var_os("CARGO_HOME") {
        return Ok(PathBuf::from(path));
    }
    if let Some(home) = dirs::home_dir() {
        return Ok(home.join(".cargo"));
    }
    bail!("failed to resolve cargo install root from CARGO_INSTALL_ROOT, CARGO_HOME, or home directory")
}

fn select_bins(
    discovered_bins: &BTreeMap<String, PathBuf>,
    requested_bins: &[String],
) -> Result<Vec<String>> {
    if requested_bins.is_empty() {
        return Ok(discovered_bins.keys().cloned().collect());
    }

    let available: BTreeSet<&str> = discovered_bins.keys().map(String::as_str).collect();
    let mut missing = Vec::new();
    let mut selected = Vec::new();
    let mut seen = BTreeSet::new();

    for bin in requested_bins {
        if !available.contains(bin.as_str()) {
            missing.push(bin.clone());
            continue;
        }
        if seen.insert(bin.clone()) {
            selected.push(bin.clone());
        }
    }

    if !missing.is_empty() {
        bail!(
            "unknown bin name(s): {}. Available bins: {}",
            missing.join(", "),
            discovered_bins
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    Ok(selected)
}

fn discover_bins(metadata: &Metadata) -> Result<BTreeMap<String, PathBuf>> {
    let workspace_members = &metadata.workspace_members;
    let mut bins = BTreeMap::new();

    for package in &metadata.packages {
        if !workspace_members.contains(package.id.as_str())
            || package.publish.as_ref().is_some_and(Vec::is_empty)
        {
            continue;
        }

        for target in &package.targets {
            if target.kind.iter().any(|kind| kind == "bin") {
                let manifest_path = PathBuf::from(&package.manifest_path);
                if bins.insert(target.name.clone(), manifest_path).is_some() {
                    let name = &target.name;
                    bail!(
                        "duplicate bin target name `{name}` found in multiple workspace packages"
                    );
                }
            }
        }
    }

    if bins.is_empty() {
        bail!("no workspace binary targets found");
    }

    Ok(bins)
}

fn cargo_metadata() -> Result<Metadata> {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .output()
        .context("failed to run cargo metadata")?;

    if !output.status.success() {
        return Err(command_failure(
            "cargo metadata",
            output.status,
            &output.stderr,
        ));
    }

    parse_metadata(&output.stdout)
}

fn parse_metadata(stdout: &[u8]) -> Result<Metadata> {
    let root: Value =
        serde_json::from_slice(stdout).context("failed to parse cargo metadata output")?;

    let workspace_members = root
        .get("workspace_members")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("cargo metadata missing workspace_members array"))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("workspace member id must be string"))
        })
        .collect::<Result<BTreeSet<_>>>()?;
    let target_directory = root
        .get("target_directory")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("cargo metadata missing string target_directory"))?;

    let packages = root
        .get("packages")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("cargo metadata missing packages array"))?
        .iter()
        .map(parse_package)
        .collect::<Result<Vec<_>>>()?;

    Ok(Metadata {
        packages,
        target_directory,
        workspace_members,
    })
}

fn parse_package(value: &Value) -> Result<MetadataPackage> {
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("package missing string id"))?
        .to_owned();
    let manifest_path = value
        .get("manifest_path")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("package missing string manifest_path"))?
        .to_owned();
    let publish = match value.get("publish") {
        Some(Value::Null) | None => None,
        Some(value) => Some(
            value
                .as_array()
                .ok_or_else(|| anyhow!("package publish field must be array or null"))?
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| anyhow!("package publish entry must be string"))
                })
                .collect::<Result<Vec<_>>>()?,
        ),
    };
    let targets = value
        .get("targets")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("package missing targets array"))?
        .iter()
        .map(parse_target)
        .collect::<Result<Vec<_>>>()?;

    Ok(MetadataPackage {
        id,
        manifest_path,
        publish,
        targets,
    })
}

fn parse_target(value: &Value) -> Result<MetadataTarget> {
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("target missing string name"))?
        .to_owned();
    let kind = value
        .get("kind")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("target missing kind array"))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("target kind must be string"))
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(MetadataTarget { name, kind })
}

fn run_command(command: &mut Command, label: &str) -> Result<()> {
    let status = command
        .status()
        .with_context(|| format!("failed to run {label}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(command_failure(label, status, &[]))
    }
}

fn command_failure(label: &str, status: ExitStatus, stderr: &[u8]) -> anyhow::Error {
    let detail = String::from_utf8_lossy(stderr).trim().to_owned();
    if detail.is_empty() {
        anyhow!("{label} exited with status {status}")
    } else {
        anyhow!("{label} exited with status {status}: {detail}")
    }
}

fn print_help() {
    println!("{}", help_text());
}

fn print_install_help() {
    println!("{}", install_help_text());
}

fn help_text() -> &'static str {
    "Workspace automation tasks\n\nUsage: cargo xtask <COMMAND>\n\nCommands:\n  install    Build and copy workspace binaries into cargo bin dir\n  help       Print this message or help for given subcommand\n\nOptions:\n  -h, --help  Print help"
}

fn install_help_text() -> &'static str {
    "Build and copy workspace binaries into cargo bin dir\n\nUsage: cargo xtask install [OPTIONS] [BINS]...\n\nArguments:\n  [BINS]...  Restrict install to one or more workspace binary names\n\nOptions:\n      --debug  Install debug build instead of default release build\n  -h, --help   Print help"
}

struct Metadata {
    packages: Vec<MetadataPackage>,
    target_directory: PathBuf,
    workspace_members: BTreeSet<String>,
}

struct MetadataPackage {
    id: String,
    manifest_path: String,
    publish: Option<Vec<String>>,
    targets: Vec<MetadataTarget>,
}

struct MetadataTarget {
    name: String,
    kind: Vec<String>,
}
