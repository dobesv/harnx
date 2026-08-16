mod cli;
mod commands;
mod credentials;
mod fetch;
mod install;
mod semver_util;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Command};
use harnx_core::logging::LogSink;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // `-v` raises the level rather than replacing it, so `HARNX_LOG_LEVEL` still
    // sets the floor for this binary the way it does for every other one.
    let mut settings = harnx_core::logging::settings(LogSink::Stderr);
    settings.level = settings.level.max(verbose_level(cli.verbose));
    let _ = harnx_core::logging::init_with(settings);

    match &cli.command {
        Command::Add(args) => commands::add::run(args).await?,
        Command::Remove(args) => commands::remove::run(args).await?,
        Command::Update(args) => commands::update::run(args).await?,
        Command::List => commands::list::run().await?,
        Command::CheckForUpdates(args) => commands::check_updates::run(args).await?,
    }

    Ok(())
}

/// Floor the `-v` flags put under the configured level. `Off` is the identity
/// for `max`, so no flag means no bump.
fn verbose_level(verbose: u8) -> log::LevelFilter {
    match verbose {
        0 => log::LevelFilter::Off,
        1 => log::LevelFilter::Debug,
        _ => log::LevelFilter::Trace,
    }
}

#[cfg(test)]
mod tests {
    use super::verbose_level;
    use log::LevelFilter;

    #[test]
    fn verbose_flags_only_ever_raise_the_level() {
        assert_eq!(LevelFilter::Info.max(verbose_level(0)), LevelFilter::Info);
        assert_eq!(LevelFilter::Info.max(verbose_level(1)), LevelFilter::Debug);
        assert_eq!(LevelFilter::Info.max(verbose_level(3)), LevelFilter::Trace);
        // An explicit HARNX_LOG_LEVEL above the flag wins.
        assert_eq!(LevelFilter::Trace.max(verbose_level(1)), LevelFilter::Trace);
    }
}
