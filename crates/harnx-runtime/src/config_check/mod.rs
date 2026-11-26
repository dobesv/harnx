//! `harnx validate`: check a config directory the way harnx would load it,
//! collecting every problem as a finding instead of stopping at the first.
//!
//! [`check_config_dir`] runs one pass per kind of check over the files
//! [`discover`] lists. Each pass reads [`CheckContext`] and pushes findings
//! into the [`Report`], which applies `--allow` and `--strict` and counts at
//! the end.

mod agents;
mod field_probe;
mod known_tools;
mod layout;
mod references;
mod report;
mod settings;
mod silent_drops;
mod tool_check;
mod tool_listing;
mod tool_refs;
mod unknown_keys;

use std::path::{Path, PathBuf, MAIN_SEPARATOR};
use std::time::Duration;

use anyhow::{bail, Result};

use crate::config::Config;

pub use agents::LoadedAgent;
pub use layout::{discover, discover_sources, ConfigLayout, ConfigSources, PackageLayout};
pub use report::{Code, Finding, Report, Severity, Summary};
pub use settings::{AllowRule, CheckSettings};
pub use tool_listing::{list_servers, server_key, ServerListing, KNOWN_TOOLSET_NAMES};
pub use tool_refs::{collect_tool_refs, ToolRef};
pub use unknown_keys::{check_files, parse_client, parse_yaml, unknown_key_findings};

/// How long one tool server may take to list its tools.
pub const DEFAULT_LIST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckOptions {
    /// The checked directory: findings are relative to it. Always
    /// `sources.root`.
    pub config_dir: PathBuf,
    /// Where each kind of file is read from.
    pub sources: ConfigSources,
    /// Access rules to check instead of `<config_dir>/access.yaml`.
    pub access_rules: Option<PathBuf>,
    /// User aliases to check instead of `<config_dir>/users.yaml`.
    pub users: Option<PathBuf>,
    /// Start tool servers to confirm the tool names agents use.
    pub check_tools: bool,
    /// Tool name globs to treat as valid.
    pub ignore_tools: Vec<String>,
    /// Fail on warnings too.
    pub strict: bool,
    /// Findings to drop before counting.
    pub allow: Vec<AllowRule>,
    /// Per tool server listing timeout.
    pub list_timeout: Duration,
    /// The directory holds only packages checked as if installed alone
    /// (`harnx validate --package`), so a model on a top-level client the
    /// installing config would provide is a warning, not an error.
    pub package_mode: bool,
}

impl CheckOptions {
    /// Check `config_dir` on its own, whatever `HARNX_CONFIG_FILE` says.
    pub fn new(config_dir: PathBuf) -> Self {
        Self::with_sources(ConfigSources::in_dir(&config_dir))
    }

    /// Check the config harnx itself would load, honouring
    /// `HARNX_CONFIG_FILE` and the other path variables.
    pub fn from_env() -> Self {
        Self::with_sources(ConfigSources::from_env())
    }

    fn with_sources(sources: ConfigSources) -> Self {
        Self {
            config_dir: sources.root.clone(),
            sources,
            access_rules: None,
            users: None,
            check_tools: true,
            ignore_tools: Vec::new(),
            strict: false,
            allow: Vec::new(),
            list_timeout: DEFAULT_LIST_TIMEOUT,
            package_mode: false,
        }
    }
}

/// What every pass reads.
pub struct CheckContext<'a> {
    pub options: &'a CheckOptions,
    pub layout: &'a ConfigLayout,
}

/// Check the files `options.sources` names. Fails only when the check can't
/// run: the directory can't be read, a file named in the options doesn't
/// exist, or an `ignore_tools` glob doesn't compile.
///
/// The loaders find packages through `HARNX_CONFIG_DIR`, so for
/// [`CheckOptions::new`] the caller points it at `config_dir` first; this
/// function changes nothing global.
pub async fn check_config_dir(options: &CheckOptions) -> Result<Report> {
    let layout = discover_with_options(options)?;
    let mut report = Report::default();
    let settings = CheckSettings {
        ignore_tools: options.ignore_tools.clone(),
        strict: options.strict,
        allow: options.allow.clone(),
    };
    let ignore = settings::ignore_set(&settings.ignore_tools)?;
    let context = CheckContext {
        options,
        layout: &layout,
    };
    check_not_empty(&context, &mut report);
    check_files(&context, &mut report);
    let mut config = check_config_loads(&context, &mut report);
    silent_drops::check_silent_drops(&context, &mut report);
    let agents = agents::check_agents(&context, config.as_mut(), &mut report);
    references::check_references(&context, config.as_ref(), &mut report);
    tool_check::check_tools(&context, config.as_ref(), &agents, &ignore, &mut report).await;
    Ok(report.finish(&settings))
}

/// The layout, with `--access-rules` and `--users` in place of the files the
/// directory holds.
fn discover_with_options(options: &CheckOptions) -> Result<ConfigLayout> {
    let mut layout = discover_sources(&options.sources)?;
    layout.access = explicit_file(&options.access_rules)?.or(layout.access);
    layout.users = explicit_file(&options.users)?.or(layout.users);
    Ok(layout)
}

fn explicit_file(path: &Option<PathBuf>) -> Result<Option<PathBuf>> {
    match path {
        Some(path) if !path.is_file() => bail!("File '{}' does not exist", path.display()),
        _ => Ok(path.clone()),
    }
}

fn check_not_empty(context: &CheckContext<'_>, report: &mut Report) {
    if context.layout.is_empty() {
        report.push(Finding::of(
            Code::EmptyConfig,
            "nothing to check: no config.yaml, agents, clients, tool servers or packages",
        ));
    }
}

/// Load the whole directory with the runtime's loader and report the first
/// thing that stops it, unless it names a file whose own error is already
/// reported: what's left is what only shows across files, such as
/// middleware that doesn't bind or two script tools with one name. Returns
/// the config when it loads, for the passes that check against it.
fn check_config_loads(context: &CheckContext<'_>, report: &mut Report) -> Option<Config> {
    let layout = context.layout;
    let error = match Config::load_from_file(&context.options.sources.config_file) {
        Ok(config) => return Some(config),
        Err(error) => error,
    };
    let message = without_root(&format!("{error:#}"), &layout.root);
    let unexplained = Some(message).filter(|message| !names_a_file_with_errors(message, report));
    report
        .findings
        .extend(unexplained.map(|message| load_finding(layout, message)));
    None
}

/// The loader's error, pinned to the config file when there is one.
fn load_finding(layout: &ConfigLayout, message: String) -> Finding {
    let finding = Finding::of(Code::ParseError, message);
    match &layout.config_file {
        Some(file) => finding.in_file(layout.relative(file)),
        None => finding,
    }
}

/// Whether `message`, with paths relative to the checked directory, names a
/// file that already has a parse error, the only per-file finding that also
/// stops the loader.
fn names_a_file_with_errors(message: &str, report: &Report) -> bool {
    report
        .findings
        .iter()
        .filter(|finding| finding.code == Code::ParseError)
        .filter_map(|finding| finding.file.as_deref())
        .any(|file| message.contains(&file.display().to_string()))
}

/// `message` with paths under `root` written relative to it.
fn without_root(message: &str, root: &Path) -> String {
    let root: PathBuf = root.components().collect();
    message.replace(&format!("{}{MAIN_SEPARATOR}", root.display()), "")
}

impl ConfigLayout {
    /// `path` relative to the checked directory, as findings report it.
    pub fn relative(&self, path: &Path) -> PathBuf {
        path.strip_prefix(&self.root)
            .map(Path::to_path_buf)
            .unwrap_or_else(|_| path.to_path_buf())
    }
}
