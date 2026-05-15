mod cli;
mod commands;
mod credentials;
mod fetch;
mod install;
mod semver_util;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Command};
use simplelog::{ColorChoice, Config, LevelFilter, TermLogger, TerminalMode};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let level = match cli.verbose {
        0 => LevelFilter::Warn,
        1 => LevelFilter::Debug,
        _ => LevelFilter::Trace,
    };
    TermLogger::init(
        level,
        Config::default(),
        TerminalMode::Mixed,
        ColorChoice::Auto,
    )
    .unwrap_or_default();

    match &cli.command {
        Command::Add(args) => commands::add::run(args).await?,
        Command::Remove(args) => commands::remove::run(args).await?,
        Command::Update(args) => commands::update::run(args).await?,
        Command::List => commands::list::run().await?,
        Command::CheckForUpdates(args) => commands::check_updates::run(args).await?,
    }

    Ok(())
}
