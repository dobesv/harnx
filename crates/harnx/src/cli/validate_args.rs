use clap::{Args, ValueEnum};
use harnx_runtime::config_check::AllowRule;
use std::path::PathBuf;

/// Check a config directory or package without starting harnx
///
/// Exits 0 when nothing is wrong (warnings allowed unless --strict), 1 when
/// errors were found, and 2 when the check couldn't run.
#[derive(Args, Debug, PartialEq, Eq)]
pub struct ValidateArgs {
    /// Config directory to check [default: the one harnx would use]
    #[arg(long, value_name = "DIR")]
    pub config_dir: Option<PathBuf>,
    /// Check this package as if installed alone into an empty config
    /// directory (repeatable)
    #[arg(long, value_name = "DIR", conflicts_with = "config_dir")]
    pub package: Vec<PathBuf>,
    /// Access rules file to check instead of <config-dir>/access.yaml
    #[arg(long, value_name = "FILE")]
    pub access_rules: Option<PathBuf>,
    /// User aliases file to check instead of <config-dir>/users.yaml
    #[arg(long, value_name = "FILE")]
    pub users: Option<PathBuf>,
    /// Start tool servers to confirm the tool names agents use, or skip that
    #[arg(long, value_enum, default_value = "check")]
    pub tools: ToolsMode,
    /// Tool name glob to treat as valid (repeatable)
    #[arg(long, value_name = "GLOB")]
    pub ignore_tool: Vec<String>,
    /// Fail on warnings too
    #[arg(long)]
    pub strict: bool,
    /// Drop findings with this code before counting, only in files matching
    /// GLOB when given (repeatable)
    #[arg(long, value_name = "CODE[:GLOB]")]
    pub allow: Vec<AllowRule>,
    /// Output format
    #[arg(long, value_enum, default_value = "text")]
    pub format: OutputFormat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ToolsMode {
    Check,
    Skip,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Text,
    Json,
}
