//! `harnx validate`: pick the directory to check, point harnx's config paths
//! at it, run the check and print the report.
//!
//! Nothing here initializes `Config` or reaches a broker: the check only
//! reads files, so it runs in CI without one.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf, MAIN_SEPARATOR};

use anyhow::{bail, Context, Result};
use harnx_core::config_paths::{get_env_name, PACKAGES_DIR_NAME};
use harnx_runtime::config_check::{check_config_dir, CheckOptions, CheckSettings, Finding, Report};
use tempfile::TempDir;

use crate::cli::{OutputFormat, ToolsMode, ValidateArgs};

/// Exit code when the check itself couldn't run, matching clap's for bad
/// arguments.
const CHECK_FAILED_EXIT_CODE: u8 = 2;

/// Run the check and print the report; returns the process exit code.
pub(crate) async fn run(args: &ValidateArgs) -> u8 {
    match check(args).await {
        Ok(report) => {
            let rendered = match args.format {
                OutputFormat::Text => report.render_text(),
                OutputFormat::Json => format!("{}\n", report.render_json()),
            };
            // A closed stdout (`| head`) leaves nothing to tell; the exit
            // code still carries the result.
            let _ = std::io::stdout().lock().write_all(rendered.as_bytes());
            report.exit_code()
        }
        Err(error) => {
            eprintln!("Error: {error:#}");
            CHECK_FAILED_EXIT_CODE
        }
    }
}

async fn check(args: &ValidateArgs) -> Result<Report> {
    let target = CheckTarget::resolve(args)?;
    let base = match &target.dir {
        Some(dir) => {
            point_config_paths_at(dir);
            CheckOptions::new(dir.clone())
        }
        None => CheckOptions::from_env(),
    };
    let mut options = options(args, base);
    options.package_mode = !target.packages.is_empty();
    let report = check_config_dir(&options).await?;
    // `--allow` globs match the paths the report prints, so they apply once
    // package findings have moved to package-relative paths.
    let allow = CheckSettings {
        allow: args.allow.clone(),
        strict: args.strict,
        ..CheckSettings::default()
    };
    Ok(target.relocate(report).finish(&allow))
}

// qual:allow(dry, boilerplate) reason: "clap's arguments are borrowed from the parsed Cli, so the options copy them"
fn options(args: &ValidateArgs, base: CheckOptions) -> CheckOptions {
    CheckOptions {
        access_rules: args.access_rules.clone(),
        users: args.users.clone(),
        check_tools: args.tools == ToolsMode::Check,
        ignore_tools: args.ignore_tool.clone(),
        strict: args.strict,
        ..base
    }
}

/// The loaders resolve packages and patch files from `HARNX_CONFIG_DIR`, so
/// it has to name the checked directory for the rest of the process.
/// `HARNX_CONFIG_FILE` would move the config file and its sibling
/// directories elsewhere.
fn point_config_paths_at(dir: &Path) {
    std::env::set_var(get_env_name("config_dir"), dir);
    std::env::remove_var(get_env_name("config_file"));
}

/// What to check: a directory given on the command line, a temporary config
/// directory holding only the `--package` packages (removed when this is
/// dropped), or, with neither, the config harnx itself would load.
struct CheckTarget {
    /// `None` when checking the config harnx would load, found through
    /// harnx's environment variables, which are left alone.
    dir: Option<PathBuf>,
    /// The names the `--package` packages are installed under.
    packages: Vec<String>,
    _temp: Option<TempDir>,
}

impl CheckTarget {
    fn resolve(args: &ValidateArgs) -> Result<Self> {
        if !args.package.is_empty() {
            return link_packages(&args.package);
        }
        let dir = match &args.config_dir {
            Some(dir) => Some(absolute(dir)?),
            None => None,
        };
        Ok(Self {
            dir,
            packages: Vec::new(),
            _temp: None,
        })
    }

    /// Report package files relative to the package rather than to the
    /// temporary directory: `agents/x.md` for a single package, and
    /// `<package>/agents/x.md` when several were given.
    fn relocate(&self, mut report: Report) -> Report {
        for package in &self.packages {
            let from = Path::new(PACKAGES_DIR_NAME).join(package);
            let to = match self.packages.len() {
                1 => PathBuf::new(),
                _ => PathBuf::from(package),
            };
            for finding in &mut report.findings {
                relocate_finding(finding, &from, &to);
            }
        }
        report
    }
}

fn relocate_finding(finding: &mut Finding, from: &Path, to: &Path) {
    if let Some(rest) = finding
        .file
        .as_deref()
        .and_then(|file| file.strip_prefix(from).ok())
    {
        finding.file = Some(to.join(rest));
    }
    let replacement = match to.as_os_str().is_empty() {
        true => String::new(),
        false => format!("{}{MAIN_SEPARATOR}", to.display()),
    };
    finding.message = finding
        .message
        .replace(&format!("{}{MAIN_SEPARATOR}", from.display()), &replacement);
}

/// `dir` made absolute with any trailing separator dropped, so paths the
/// loaders print start with exactly this prefix.
fn absolute(dir: &Path) -> Result<PathBuf> {
    let dir = std::path::absolute(dir)
        .with_context(|| format!("Cannot resolve config directory '{}'", dir.display()))?;
    Ok(dir.components().collect())
}

/// A temporary config directory with each package under `packages/<name>`.
fn link_packages(packages: &[PathBuf]) -> Result<CheckTarget> {
    let temp = tempfile::tempdir().context("Failed to create a temporary config directory")?;
    let packages_dir = temp.path().join(PACKAGES_DIR_NAME);
    fs::create_dir(&packages_dir)
        .with_context(|| format!("Failed to create '{}'", packages_dir.display()))?;
    let mut linked: Vec<String> = Vec::new();
    for package in packages {
        let (source, name) = package_source(package)?;
        if linked.contains(&name) {
            bail!("Two packages are named '{name}'");
        }
        link_package(&source, &packages_dir.join(&name))?;
        linked.push(name);
    }
    Ok(CheckTarget {
        dir: Some(temp.path().to_path_buf()),
        packages: linked,
        _temp: Some(temp),
    })
}

/// The package's resolved directory and the name it installs under: the last
/// component of `package` as given, since harnx names a package after its
/// entry under `packages/` even when that entry is a link. A path with no
/// such component, like `.` or `..`, is named after the directory it
/// resolves to.
fn package_source(package: &Path) -> Result<(PathBuf, String)> {
    let source = fs::canonicalize(package)
        .with_context(|| format!("Package directory '{}' does not exist", package.display()))?;
    if !source.is_dir() {
        bail!("Package '{}' is not a directory", package.display());
    }
    let name = package
        .file_name()
        .or_else(|| source.file_name())
        .and_then(|name| name.to_str())
        .filter(|name| !name.starts_with('.'))
        .map(str::to_string)
        .with_context(|| {
            format!(
                "Package directory '{}' has no usable name; harnx skips hidden and non-UTF-8 package directories",
                package.display()
            )
        })?;
    Ok((source, name))
}

#[cfg(unix)]
fn link_package(source: &Path, target: &Path) -> Result<()> {
    std::os::unix::fs::symlink(source, target)
        .with_context(|| format!("Failed to link package '{}'", source.display()))
}

/// Creating a directory symlink on Windows needs a privilege CI runners and
/// most users lack, so the package is copied instead.
#[cfg(not(unix))]
fn link_package(source: &Path, target: &Path) -> Result<()> {
    copy_dir(source, target)
        .with_context(|| format!("Failed to copy package '{}'", source.display()))
}

#[cfg(not(unix))]
fn copy_dir(source: &Path, target: &Path) -> std::io::Result<()> {
    fs::create_dir_all(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let destination = target.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &destination)?;
        } else {
            fs::copy(entry.path(), &destination)?;
        }
    }
    Ok(())
}
