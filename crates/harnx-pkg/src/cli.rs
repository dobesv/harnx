use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "harnx-pkg", about = "Manage harnx packages", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
    /// Verbosity (-v for debug, -vv for trace)
    #[clap(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Install a package from git or OCI
    Add(AddArgs),
    /// Remove an installed package
    Remove(RemoveArgs),
    /// Update an installed package to a newer version
    Update(UpdateArgs),
    /// List installed packages
    List,
    /// Check for available updates without installing
    CheckForUpdates(CheckForUpdatesArgs),
}

#[derive(Args, Debug)]
pub struct AddArgs {
    /// Package source URL (git: https://... or file://..., OCI: ghcr.io/... or oci://...)
    pub url: String,
    /// Version tag to install (must be semver, e.g. v1.2.3)
    pub tag: String,
    /// Optional name override (defaults to repo/image name)
    #[clap(long)]
    pub name: Option<String>,
    /// Only copy files from this subpath in the repo/image
    #[clap(long)]
    pub subpath: Option<String>,
}

#[derive(Args, Debug)]
pub struct RemoveArgs {
    /// Name of the installed package to remove
    pub name: String,
}

#[derive(Args, Debug)]
pub struct UpdateArgs {
    /// Name of the installed package to update (omit to update all)
    pub name: Option<String>,
}

#[derive(Args, Debug)]
pub struct CheckForUpdatesArgs {
    /// Name of the installed package to check (omit to check all)
    pub name: Option<String>,
}
