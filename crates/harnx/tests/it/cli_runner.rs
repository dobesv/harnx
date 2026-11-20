//! Run the real `harnx` binary against a per-fixture config directory.

use std::path::Path;
use std::process::{Command, Output};

use anyhow::{Context, Result};

/// Run the CLI with the fixture's config/data/state directories and the
/// fixture NATS cluster, plus any extra environment entries. `HARNX_NATS_URL`
/// and `HARNX_NATS_TOKEN` are removed so the cluster config file decides.
pub(crate) fn run_harnx(
    config_dir: &Path,
    args: &[&str],
    extra_env: &[(&str, &str)],
) -> Result<Output> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_harnx"));
    command
        .args(args)
        .env("HARNX_CONFIG_DIR", config_dir.join("config"))
        .env("HARNX_DATA_DIR", config_dir.join("data"))
        .env("HARNX_STATE_DIR", config_dir.join("state"))
        .env("HARNX_NATS_SERVER", "fixture")
        .env_remove("HARNX_NATS_URL")
        .env_remove("HARNX_NATS_TOKEN");
    for (key, value) in extra_env {
        command.env(key, value);
    }
    command.output().context("run harnx CLI")
}
