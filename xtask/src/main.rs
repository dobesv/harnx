use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
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
    skip_web: bool,
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
    let mut skip_web = false;
    let mut bins = Vec::new();

    for arg in args {
        match arg.to_str() {
            Some("--debug") => debug = true,
            Some("--skip-web") => skip_web = true,
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

    Ok(InstallArgs {
        debug,
        skip_web,
        bins,
    })
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

    if args.skip_web {
        println!("==> Skipping web-ui build (--skip-web)");
    } else {
        install_web_assets(&metadata.workspace_root)?;
    }

    Ok(())
}

/// Build the web UI and copy the compiled assets into the directory that
/// `harnx serve` loads from by default (`<data_dir>/web-assets`).
///
/// The web project lives in `web/` at the workspace root and builds with pnpm
/// (`pnpm install` + `pnpm build`), emitting static files into `web/dist`.
fn install_web_assets(workspace_root: &Path) -> Result<()> {
    let web_dir = workspace_root.join("web");
    if !web_dir.join("package.json").is_file() {
        bail!(
            "web project not found at {} (missing package.json); pass --skip-web to skip",
            web_dir.display()
        );
    }

    let pnpm = pnpm_command();
    ensure_pnpm_available(&pnpm)?;

    println!("==> Installing web-ui dependencies (pnpm install)");
    let mut install = Command::new(&pnpm);
    install
        .arg("install")
        .arg("--frozen-lockfile")
        .current_dir(&web_dir);
    run_command(&mut install, "pnpm install")?;

    println!("==> Building web-ui (pnpm build)");
    let mut build = Command::new(&pnpm);
    build.arg("build").current_dir(&web_dir);
    run_command(&mut build, "pnpm build")?;

    let dist = web_dir.join("dist");
    if !dist.is_dir() {
        bail!(
            "web build did not produce a dist directory at {}",
            dist.display()
        );
    }

    let destination = harnx_data_dir()?.join("web-assets");
    println!("==> Installing web-ui assets -> {}", destination.display());
    replace_dir(&dist, &destination)?;

    Ok(())
}

/// Resolve the pnpm executable, honoring an override via `PNPM` for
/// environments where pnpm is invoked through a wrapper (e.g. corepack).
fn pnpm_command() -> OsString {
    env::var_os("PNPM").unwrap_or_else(|| OsString::from("pnpm"))
}

/// Verify the pnpm executable can be run before we lean on it, so a missing
/// pnpm yields an actionable message instead of a raw "No such file" IO error.
fn ensure_pnpm_available(pnpm: &OsString) -> Result<()> {
    match Command::new(pnpm).arg("--version").output() {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(command_failure(
            "pnpm --version",
            output.status,
            &output.stderr,
        )),
        Err(err) => Err(anyhow::Error::from(err).context(format!(
            "failed to run `{}` — install pnpm (https://pnpm.io/) or pass --skip-web \
             to install without the web UI",
            pnpm.to_string_lossy()
        ))),
    }
}

/// Root data directory, mirroring `harnx_core::config_paths::data_dir()`:
/// 1. `HARNX_DATA_DIR` env var (literal path).
/// 2. `XDG_DATA_HOME/harnx` (XDG override).
/// 3. OS default (`dirs::data_dir()/harnx`).
///
/// Reimplemented locally to keep `xtask` free of a `harnx-core` dependency.
fn harnx_data_dir() -> Result<PathBuf> {
    if let Some(v) = env::var_os("HARNX_DATA_DIR") {
        if !v.is_empty() {
            return Ok(PathBuf::from(v));
        }
    }
    if let Some(v) = env::var_os("XDG_DATA_HOME") {
        if !v.is_empty() {
            return Ok(PathBuf::from(v).join("harnx"));
        }
    }
    dirs::data_dir()
        .map(|dir| dir.join("harnx"))
        .ok_or_else(|| {
            anyhow!("failed to resolve data dir from HARNX_DATA_DIR, XDG_DATA_HOME, or OS default")
        })
}

/// Replace `destination` with a fresh copy of `source`, keeping stale files
/// from previous builds from lingering.
///
/// The copy lands in a sibling staging directory first; only once it is fully
/// populated do we swap it into place with a double rename
/// (`destination -> <destination>.old`, then `staging -> destination`). Renames
/// are near-instant and, on Windows, moving a directory that holds open files
/// tends to succeed where deleting it in place would fail — so a concurrent
/// `harnx serve` is very unlikely to observe a missing or half-written asset
/// directory.
fn replace_dir(source: &Path, destination: &Path) -> Result<()> {
    let parent = destination
        .parent()
        .ok_or_else(|| anyhow!("destination {} has no parent dir", destination.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create dir {}", parent.display()))?;

    let file_name = destination
        .file_name()
        .ok_or_else(|| anyhow!("destination {} has no file name", destination.display()))?;
    let staging = sibling_with_suffix(parent, file_name, ".new");
    let backup = sibling_with_suffix(parent, file_name, ".old");

    // Clean up leftovers from a previous interrupted run.
    remove_dir_if_exists(&staging)
        .with_context(|| format!("failed to clear stale staging dir {}", staging.display()))?;
    remove_dir_if_exists(&backup)
        .with_context(|| format!("failed to clear stale backup dir {}", backup.display()))?;

    if let Err(err) = copy_dir_recursive(source, &staging) {
        let _ = fs::remove_dir_all(&staging);
        return Err(err);
    }

    // Move any existing install aside first so the destination is only briefly
    // absent (between two renames) rather than during a full delete.
    let had_existing = destination.exists();
    if had_existing {
        if let Err(err) = fs::rename(destination, &backup) {
            let _ = fs::remove_dir_all(&staging);
            return Err(err).with_context(|| {
                format!(
                    "failed to move existing dir {} aside to {}",
                    destination.display(),
                    backup.display()
                )
            });
        }
    }

    if let Err(err) = fs::rename(&staging, destination) {
        // Roll back: restore the previous install if we moved it aside.
        if had_existing {
            let _ = fs::rename(&backup, destination);
        }
        let _ = fs::remove_dir_all(&staging);
        return Err(err).with_context(|| {
            format!(
                "failed to move staged assets {} into place at {}",
                staging.display(),
                destination.display()
            )
        });
    }

    // Best-effort cleanup of the superseded install.
    let _ = fs::remove_dir_all(&backup);
    Ok(())
}

/// Build a sibling path in `parent` named `.<file_name><suffix>`.
fn sibling_with_suffix(parent: &Path, file_name: &std::ffi::OsStr, suffix: &str) -> PathBuf {
    let mut name = OsString::from(".");
    name.push(file_name);
    name.push(suffix);
    parent.join(name)
}

/// Remove `dir` recursively if it exists, treating a missing dir as success.
fn remove_dir_if_exists(dir: &Path) -> Result<()> {
    match fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

/// Recursively copy the contents of `source` into `destination`.
fn copy_dir_recursive(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)
        .with_context(|| format!("failed to create dir {}", destination.display()))?;
    for entry in
        fs::read_dir(source).with_context(|| format!("failed to read dir {}", source.display()))?
    {
        let entry =
            entry.with_context(|| format!("failed to read entry in {}", source.display()))?;
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to stat {}", entry.path().display()))?;
        let target = destination.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target).with_context(|| {
                format!(
                    "failed to copy {} to {}",
                    entry.path().display(),
                    target.display()
                )
            })?;
        }
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
    let workspace_root = root
        .get("workspace_root")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("cargo metadata missing string workspace_root"))?;

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
        workspace_root,
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
    "Build and copy workspace binaries into cargo bin dir\n\nAlso builds the web UI (pnpm) and copies it to the default web-assets\ndirectory that `harnx serve` loads from (<data_dir>/web-assets).\n\nUsage: cargo xtask install [OPTIONS] [BINS]...\n\nArguments:\n  [BINS]...  Restrict install to one or more workspace binary names\n\nOptions:\n      --debug     Install debug build instead of default release build\n      --skip-web  Skip building and installing the web UI assets\n  -h, --help      Print help"
}

struct Metadata {
    packages: Vec<MetadataPackage>,
    target_directory: PathBuf,
    workspace_root: PathBuf,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::id;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Serialize env-mutating tests: process-global env is shared across the
    /// binary, so concurrent mutation would race.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn unique_tmp(label: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        env::temp_dir().join(format!("xtask-{label}-{}-{n}", id()))
    }

    struct EnvVarGuard {
        key: &'static str,
        prev: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let prev = env::var_os(key);
            env::set_var(key, value);
            Self { key, prev }
        }

        fn unset(key: &'static str) -> Self {
            let prev = env::var_os(key);
            env::remove_var(key);
            Self { key, prev }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => env::set_var(self.key, v),
                None => env::remove_var(self.key),
            }
        }
    }

    #[test]
    fn harnx_data_dir_prefers_explicit_env() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _xdg = EnvVarGuard::unset("XDG_DATA_HOME");
        let _data = EnvVarGuard::set("HARNX_DATA_DIR", "/tmp/explicit-data");
        assert_eq!(
            harnx_data_dir().unwrap(),
            PathBuf::from("/tmp/explicit-data")
        );
    }

    #[test]
    fn harnx_data_dir_uses_xdg_override() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _data = EnvVarGuard::unset("HARNX_DATA_DIR");
        let _xdg = EnvVarGuard::set("XDG_DATA_HOME", "/tmp/xdg-data");
        assert_eq!(
            harnx_data_dir().unwrap(),
            PathBuf::from("/tmp/xdg-data").join("harnx")
        );
    }

    #[test]
    fn copy_dir_recursive_copies_nested_tree() {
        let src = unique_tmp("copy-src");
        let dst = unique_tmp("copy-dst");
        fs::create_dir_all(src.join("assets")).unwrap();
        fs::write(src.join("index.html"), b"<html>").unwrap();
        fs::write(src.join("assets").join("app.js"), b"console.log(1)").unwrap();

        copy_dir_recursive(&src, &dst).unwrap();

        assert_eq!(fs::read(dst.join("index.html")).unwrap(), b"<html>");
        assert_eq!(
            fs::read(dst.join("assets").join("app.js")).unwrap(),
            b"console.log(1)"
        );

        fs::remove_dir_all(&src).ok();
        fs::remove_dir_all(&dst).ok();
    }

    #[test]
    fn replace_dir_clears_stale_files() {
        let src = unique_tmp("replace-src");
        let dst = unique_tmp("replace-dst");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("new.txt"), b"new").unwrap();

        // Pre-populate destination with a stale file that must not survive.
        fs::create_dir_all(&dst).unwrap();
        fs::write(dst.join("stale.txt"), b"stale").unwrap();

        replace_dir(&src, &dst).unwrap();

        assert!(dst.join("new.txt").is_file());
        assert!(!dst.join("stale.txt").exists());

        fs::remove_dir_all(&src).ok();
        fs::remove_dir_all(&dst).ok();
    }

    #[test]
    fn replace_dir_swaps_over_existing_install_without_residue() {
        let base = unique_tmp("replace-base");
        let src = base.join("src");
        let dst = base.join("web-assets");
        fs::create_dir_all(src.join("assets")).unwrap();
        fs::write(src.join("index.html"), b"v2").unwrap();
        fs::write(src.join("assets").join("app.js"), b"v2js").unwrap();

        // Existing install with a stale file that must be gone afterwards.
        fs::create_dir_all(&dst).unwrap();
        fs::write(dst.join("old.html"), b"v1").unwrap();

        replace_dir(&src, &dst).unwrap();

        assert_eq!(fs::read(dst.join("index.html")).unwrap(), b"v2");
        assert_eq!(
            fs::read(dst.join("assets").join("app.js")).unwrap(),
            b"v2js"
        );
        assert!(!dst.join("old.html").exists());
        // No leftover staging/backup siblings.
        assert!(!base.join(".web-assets.new").exists());
        assert!(!base.join(".web-assets.old").exists());

        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn harnx_data_dir_falls_back_to_os_default() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _data = EnvVarGuard::unset("HARNX_DATA_DIR");
        let _xdg = EnvVarGuard::unset("XDG_DATA_HOME");
        // On CI/dev hosts dirs::data_dir() resolves; assert the harnx suffix.
        if let Some(expected) = dirs::data_dir() {
            assert_eq!(harnx_data_dir().unwrap(), expected.join("harnx"));
        }
    }

    #[test]
    fn parse_install_args_accepts_skip_web() {
        let args = parse_install_args(&[OsString::from("--skip-web")]).unwrap();
        assert!(args.skip_web);
        assert!(!args.debug);
        assert!(args.bins.is_empty());
    }

    #[test]
    fn parse_install_args_default_builds_web() {
        let args = parse_install_args(&[]).unwrap();
        assert!(!args.skip_web);
    }

    #[test]
    fn parse_metadata_reads_workspace_root() {
        let json = br#"{
            "workspace_members": ["pkg 0.1.0 (path+file:///w)"],
            "target_directory": "/w/target",
            "workspace_root": "/w",
            "packages": []
        }"#;
        let metadata = parse_metadata(json).unwrap();
        assert_eq!(metadata.workspace_root, PathBuf::from("/w"));
        assert_eq!(metadata.target_directory, PathBuf::from("/w/target"));
    }

    #[test]
    fn install_help_mentions_web_and_skip_flag() {
        let help = install_help_text();
        assert!(help.contains("web UI"));
        assert!(help.contains("--skip-web"));
    }
}
