//! `cargo xtask quality` fails when a change adds rustqual findings or makes
//! existing ones worse, compared with the change's merge base.
//!
//! rustqual analyses a whole tree. Its own regression gate (`--compare` with
//! `--fail-on-regression`) compares one score for the whole workspace, which
//! a well-factored function added anywhere can lift enough to hide a new
//! problem elsewhere, so this analyses both trees and compares the findings
//! themselves.
//!
//! `cargo xtask rustqual [ARGS]` runs the same pinned rustqual directly, for
//! the details behind a finding.

mod findings;
mod trees;

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};

use crate::{cargo_metadata, command_failure, executable_name, git, run_command};
use findings::{Comparison, Finding, Regression};
use trees::{ScratchDir, Trees};

/// The crate that pins rustqual's version; see the comment in it.
const RUSTQUAL_MANIFEST: &str = "xtask/rustqual/Cargo.toml";
const CONFIG: &str = "rustqual.toml";

pub struct QualityArgs {
    base: Option<String>,
    head: Option<String>,
}

pub fn parse_quality_args(args: &[OsString]) -> Result<QualityArgs> {
    let mut parsed = QualityArgs {
        base: None,
        head: None,
    };
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let slot = match arg.to_str() {
            Some("--base") => &mut parsed.base,
            Some("--head") => &mut parsed.head,
            Some("-h" | "--help") => {
                println!("{}", quality_help_text());
                std::process::exit(0);
            }
            Some(other) => bail!("unexpected quality argument `{other}`"),
            None => bail!("quality arguments must be valid UTF-8"),
        };
        let value = args
            .next()
            .and_then(|value| value.to_str())
            .ok_or_else(|| anyhow!("{} needs a revision", arg.to_string_lossy()))?;
        *slot = Some(value.to_owned());
    }
    Ok(parsed)
}

pub fn quality(args: QualityArgs) -> Result<()> {
    let root = git::toplevel()?;
    let head = args.head.as_deref();
    let base = merge_base(&root, args.base.as_deref(), head.unwrap_or("HEAD"))?;
    let rustqual = build_rustqual(&root)?;
    let scratch = ScratchDir::create("quality")?;
    let trees = Trees::export(&root, &base, head, scratch.path())?;
    let config = root.join(CONFIG);
    // rustqual analyses on one thread, so the two trees run side by side.
    let base_run = start_analysis(&rustqual, &config, &trees.base)?;
    let head_run = start_analysis(&rustqual, &config, &trees.head)?;
    let old = finish_analysis(base_run)?;
    let new = finish_analysis(head_run)?;
    let renames = trees::renames(&root, &base, head)?;
    let comparison = findings::compare(&old, &new, &renames);
    print_comparison(&comparison, short(&base));
    match comparison.regressions.len() {
        0 => Ok(()),
        1 => bail!("1 rustqual finding added or made worse"),
        count => bail!("{count} rustqual findings added or made worse"),
    }
}

/// `cargo xtask rustqual [ARGS]`: runs the pinned rustqual in the current
/// directory, with the workspace's settings unless ARGS name a config.
pub fn rustqual(args: Vec<OsString>) -> Result<()> {
    let root = git::toplevel()?;
    let mut command = Command::new(build_rustqual(&root)?);
    if !names_config(&args) {
        command.arg("--config").arg(root.join(CONFIG));
    }
    let status = command
        .args(&args)
        .status()
        .context("failed to run rustqual")?;
    if !status.success() {
        // rustqual's exit code says whether it found something (1) or could
        // not read its config (2), so pass it on.
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

fn merge_base(root: &Path, base: Option<&str>, head: &str) -> Result<String> {
    match base {
        Some(base) => Ok(git::output(root, &["merge-base", base, head])?
            .trim()
            .to_owned()),
        None => git::default_base(root, head),
    }
}

/// Builds the pinned rustqual and returns its path. It isn't a workspace
/// member, so it gets a target directory of its own. The first build takes
/// about a minute.
fn build_rustqual(root: &Path) -> Result<PathBuf> {
    let target_dir = cargo_metadata()?.target_directory.join("rustqual");
    let mut build = Command::new("cargo");
    build
        .current_dir(root)
        .args(["build", "--release", "--locked", "--manifest-path"])
        .arg(RUSTQUAL_MANIFEST)
        .arg("--target-dir")
        .arg(&target_dir);
    run_command(&mut build, "cargo build (rustqual)")?;
    Ok(target_dir.join("release").join(executable_name("rustqual")))
}

/// Starts rustqual on `tree`. It runs from inside the tree with `.` as its
/// path because it skips any file whose path has a component named `target`
/// or starting with a dot, and the scratch directory's own path may have one.
fn start_analysis(rustqual: &Path, config: &Path, tree: &Path) -> Result<Child> {
    let mut command = Command::new(rustqual);
    command
        .current_dir(tree)
        .args([".", "--no-fail", "--format", "ai-json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if config.is_file() {
        command.arg("--config").arg(config);
    }
    command.spawn().context("failed to start rustqual")
}

fn finish_analysis(run: Child) -> Result<Vec<Finding>> {
    let output = run
        .wait_with_output()
        .context("failed to wait for rustqual")?;
    if !output.status.success() {
        return Err(command_failure("rustqual", output.status, &output.stderr));
    }
    findings::parse_report(&output.stdout)
}

fn names_config(args: &[OsString]) -> bool {
    args.iter().any(|arg| {
        arg.to_str()
            .is_some_and(|arg| arg == "-c" || arg == "--config" || arg.starts_with("--config="))
    })
}

fn print_comparison(comparison: &Comparison, base: &str) {
    let annotate = env::var_os("GITHUB_ACTIONS").is_some_and(|value| value == "true");
    for regression in &comparison.regressions {
        let message = describe(regression);
        let finding = &regression.finding;
        if annotate {
            // A workflow command, which GitHub shows on the pull request's diff.
            println!(
                "::error file={},line={}::{}",
                finding.file,
                finding.line,
                escape_workflow_message(&message)
            );
        } else {
            println!("{}:{}  {message}", finding.file, finding.line);
        }
    }
    println!(
        "rustqual against {base}: {} added or made worse, {} resolved",
        comparison.regressions.len(),
        comparison.resolved
    );
    if !comparison.regressions.is_empty() {
        println!(
            "Fix them, or mark a deliberate exception with `// qual:allow(<dimension>)` and a \
             reason (see AGENTS.md). `cargo xtask rustqual <path> --verbose` shows the details."
        );
    }
}

fn describe(regression: &Regression) -> String {
    let finding = &regression.finding;
    let mut message = format!("{}: {}", finding.category, finding.detail);
    if !finding.function.is_empty() && !finding.detail.contains(&finding.function) {
        message.push_str(&format!(" in {}", finding.function));
    }
    if let Some(was) = regression.was {
        message.push_str(&format!(" (was {was})"));
    }
    message
}

fn escape_workflow_message(message: &str) -> String {
    message
        .replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

fn short(revision: &str) -> &str {
    revision.get(..12).unwrap_or(revision)
}

pub fn quality_help_text() -> &'static str {
    "Fail when a change adds rustqual findings or makes existing ones worse\n\nCompares the working tree (or --head) with its merge base, using the\nsettings in rustqual.toml for both. The first run builds rustqual.\n\nUsage: cargo xtask quality [OPTIONS]\n\nOptions:\n      --base <REV>  Compare with the merge base of REV [default: origin/HEAD]\n      --head <REV>  Analyse REV instead of the working tree\n  -h, --help        Print help"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_config_named_in_the_arguments_is_kept() {
        let args = |list: &[&str]| list.iter().map(OsString::from).collect::<Vec<_>>();

        assert!(names_config(&args(&["--config", "other.toml"])));
        assert!(names_config(&args(&["--config=other.toml"])));
        assert!(names_config(&args(&["-c", "other.toml", "src"])));
        assert!(!names_config(&args(&["crates/harnx-core", "--verbose"])));
    }

    #[test]
    fn workflow_messages_escape_line_breaks() {
        assert_eq!(escape_workflow_message("50% of\nlines"), "50%25 of%0Alines");
    }
}
