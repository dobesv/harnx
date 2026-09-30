//! `cargo xtask affected`: print the cargo package arguments that select the
//! tests a change can break, e.g. `cargo nextest run $(cargo xtask affected)`.
//!
//! guppy's determinator maps the changed files to packages and diffs the
//! dependency graph at the base commit against the working tree. The extra
//! edges it cannot see live in `.config/affected.toml`.

use std::collections::BTreeSet;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{id, Command};

use anyhow::{anyhow, bail, Context, Result};
use determinator::rules::DeterminatorRules;
use determinator::Determinator;
use globset::{Glob, GlobSet, GlobSetBuilder};
use guppy::graph::{DependencyDirection, PackageGraph};
use guppy::MetadataCommand;
use serde::Deserialize;

use crate::command_failure;

const CONFIG_PATH: &str = ".config/affected.toml";
const DEFAULT_BASE_REFS: [&str; 2] = ["origin/HEAD", "origin/main"];

pub struct AffectedArgs {
    base: Option<String>,
}

pub fn parse_affected_args(args: &[OsString]) -> Result<AffectedArgs> {
    let mut base = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--base") => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow!("--base needs a revision"))?;
                base = Some(utf8(value)?.to_owned());
            }
            Some(other) => bail!("unexpected affected argument `{other}`"),
            None => bail!("affected arguments must be valid UTF-8"),
        }
    }
    Ok(AffectedArgs { base })
}

pub fn affected(args: AffectedArgs) -> Result<()> {
    let root = git_output(Path::new("."), &["rev-parse", "--show-toplevel"])?;
    let root = PathBuf::from(root.trim());
    let base = match args.base {
        Some(base) => base,
        None => default_base(&root)?,
    };
    let config = AffectedConfig::load(&root.join(CONFIG_PATH))?;
    let changed = changed_paths(&root, &base)?;
    let new = package_graph(&root)?;
    let old = BaseCheckout::create(&root, &base)?.package_graph()?;
    let selection = select(&old, &new, &config, &changed)?;
    println!("{}", selection.cargo_args().join(" "));
    Ok(())
}

/// Which packages to test: every package, or the named ones (possibly none).
#[derive(Debug, PartialEq, Eq)]
pub enum Selection {
    Workspace,
    Packages(BTreeSet<String>),
}

impl Selection {
    pub fn cargo_args(&self) -> Vec<String> {
        match self {
            Selection::Workspace => vec!["--workspace".to_owned()],
            Selection::Packages(packages) => packages
                .iter()
                .flat_map(|name| ["-p".to_owned(), name.clone()])
                .collect(),
        }
    }
}

pub fn select(
    old: &PackageGraph,
    new: &PackageGraph,
    config: &AffectedConfig,
    changed: &[String],
) -> Result<Selection> {
    let workspace: BTreeSet<String> = new
        .workspace()
        .iter()
        .map(|package| package.name().to_owned())
        .collect();
    config.check_package_names(&workspace)?;

    let mut determinator = Determinator::new(old, new);
    determinator
        .set_rules(&config.determinator)
        .context("invalid determinator rules")?;
    determinator.add_changed_paths(changed.iter().map(String::as_str));
    let mut packages: BTreeSet<String> = determinator
        .compute()
        .affected_set
        .packages(DependencyDirection::Forward)
        .filter(|package| package.in_workspace())
        .map(|package| package.name().to_owned())
        .collect();
    let extra: Vec<String> = config.test_packages(&packages, changed).collect();
    packages.extend(extra);

    Ok(if packages.is_superset(&workspace) {
        Selection::Workspace
    } else {
        Selection::Packages(packages)
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct AffectedConfig {
    #[serde(default)]
    determinator: DeterminatorRules,
    #[serde(default)]
    test_edge: Vec<TestEdge>,
    #[serde(default)]
    test_path: Vec<TestPath>,
}

/// Tests in `test` launch a binary built by one of the `on-affected` packages.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct TestEdge {
    on_affected: Vec<String>,
    test: Vec<String>,
}

/// Tests in `test` read files matching `globs`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TestPath {
    #[serde(deserialize_with = "deserialize_globs")]
    globs: GlobSet,
    test: Vec<String>,
}

impl AffectedConfig {
    fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("invalid {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        Ok(toml::from_str(text)?)
    }

    fn test_packages<'a>(
        &'a self,
        affected: &'a BTreeSet<String>,
        changed: &'a [String],
    ) -> impl Iterator<Item = String> + 'a {
        let from_edges = self
            .test_edge
            .iter()
            .filter(|edge| edge.on_affected.iter().any(|name| affected.contains(name)))
            .flat_map(|edge| edge.test.iter());
        let from_paths = self
            .test_path
            .iter()
            .filter(|rule| changed.iter().any(|path| rule.globs.is_match(path)))
            .flat_map(|rule| rule.test.iter());
        from_edges.chain(from_paths).cloned()
    }

    /// A renamed or removed package would otherwise silently drop its edges.
    fn check_package_names(&self, workspace: &BTreeSet<String>) -> Result<()> {
        let edge_names = self
            .test_edge
            .iter()
            .flat_map(|edge| edge.on_affected.iter().chain(&edge.test));
        let path_names = self.test_path.iter().flat_map(|rule| rule.test.iter());
        let unknown: Vec<&String> = edge_names
            .chain(path_names)
            .filter(|name| !workspace.contains(*name))
            .collect();
        if unknown.is_empty() {
            Ok(())
        } else {
            bail!("{CONFIG_PATH} names packages not in the workspace: {unknown:?}")
        }
    }
}

fn deserialize_globs<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<GlobSet, D::Error> {
    let globs = Vec::<String>::deserialize(deserializer)?;
    let mut builder = GlobSetBuilder::new();
    for glob in &globs {
        builder.add(Glob::new(glob).map_err(serde::de::Error::custom)?);
    }
    builder.build().map_err(serde::de::Error::custom)
}

fn default_base(root: &Path) -> Result<String> {
    for candidate in DEFAULT_BASE_REFS {
        if let Ok(base) = git_output(root, &["merge-base", "HEAD", candidate]) {
            return Ok(base.trim().to_owned());
        }
    }
    bail!("no merge base with {DEFAULT_BASE_REFS:?}; pass --base <rev>")
}

/// Tracked changes since `base` in the working tree, plus untracked files, so
/// uncommitted work is covered too.
fn changed_paths(root: &Path, base: &str) -> Result<Vec<String>> {
    let tracked = git_output(root, &["diff", "--name-only", "--no-renames", base])?;
    let untracked = git_output(root, &["ls-files", "--others", "--exclude-standard"])?;
    Ok(tracked
        .lines()
        .chain(untracked.lines())
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}

fn package_graph(dir: &Path) -> Result<PackageGraph> {
    PackageGraph::from_command(MetadataCommand::new().current_dir(dir))
        .with_context(|| format!("failed to load the package graph in {}", dir.display()))
}

/// A detached worktree of the base commit, removed on drop.
struct BaseCheckout {
    root: PathBuf,
    path: PathBuf,
}

impl BaseCheckout {
    fn create(root: &Path, base: &str) -> Result<Self> {
        let path = env::temp_dir().join(format!("harnx-affected-base-{}", id()));
        let path_arg = utf8(path.as_os_str())?;
        git_output(
            root,
            &["worktree", "add", "--quiet", "--detach", path_arg, base],
        )?;
        Ok(Self {
            root: root.to_owned(),
            path,
        })
    }

    fn package_graph(&self) -> Result<PackageGraph> {
        package_graph(&self.path)
    }
}

impl Drop for BaseCheckout {
    fn drop(&mut self) {
        let removed = Command::new("git")
            .current_dir(&self.root)
            .args(["worktree", "remove", "--force"])
            .arg(&self.path)
            .status();
        if !matches!(removed, Ok(status) if status.success()) {
            eprintln!("warning: failed to remove worktree {}", self.path.display());
        }
    }
}

fn git_output(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .context("failed to run git")?;
    if !output.status.success() {
        let label = format!("git {}", args.join(" "));
        return Err(command_failure(&label, output.status, &output.stderr));
    }
    String::from_utf8(output.stdout).context("git output is not UTF-8")
}

fn utf8(value: &std::ffi::OsStr) -> Result<&str> {
    value
        .to_str()
        .ok_or_else(|| anyhow!("`{}` is not valid UTF-8", value.to_string_lossy()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    fn workspace_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
    }

    fn graph() -> &'static PackageGraph {
        static GRAPH: OnceLock<PackageGraph> = OnceLock::new();
        GRAPH.get_or_init(|| package_graph(&workspace_root()).expect("workspace package graph"))
    }

    fn config() -> AffectedConfig {
        AffectedConfig::load(&workspace_root().join(CONFIG_PATH)).expect("affected config")
    }

    /// Selection for `changed` against an unchanged dependency graph.
    fn select_paths(changed: &[&str]) -> Selection {
        let changed: Vec<String> = changed.iter().map(|path| (*path).to_owned()).collect();
        select(graph(), graph(), &config(), &changed).expect("selection")
    }

    fn packages(selection: Selection) -> BTreeSet<String> {
        match selection {
            Selection::Packages(packages) => packages,
            Selection::Workspace => panic!("expected a package subset, got the whole workspace"),
        }
    }

    #[test]
    fn checked_in_config_names_only_workspace_packages() {
        let workspace = graph()
            .workspace()
            .iter()
            .map(|package| package.name().to_owned())
            .collect();
        config()
            .check_package_names(&workspace)
            .expect("known package names");
    }

    #[test]
    fn unknown_package_in_config_is_rejected() {
        let config = AffectedConfig::parse(
            "[[test-edge]]\non-affected = [\"harnx-renamed\"]\ntest = [\"harnx\"]\n",
        )
        .expect("parse");
        let workspace = BTreeSet::from(["harnx".to_owned()]);
        let error = config.check_package_names(&workspace).unwrap_err();
        assert!(error.to_string().contains("harnx-renamed"), "{error}");
    }

    #[test]
    fn leaf_crate_change_adds_the_tests_that_launch_its_binary() {
        let selected = packages(select_paths(&["crates/harnx-fs-tools/src/lib.rs"]));
        assert!(selected.contains("harnx-fs-tools"), "{selected:?}");
        assert!(selected.contains("harnx-runtime"), "{selected:?}");
        assert!(!selected.contains("harnx-grep-tools"), "{selected:?}");
    }

    #[test]
    fn test_only_edges_do_not_propagate_to_dependents() {
        let selected = packages(select_paths(&["crates/harnx-fs-tools/src/lib.rs"]));
        // harnx-serve depends on harnx-runtime; a test edge must not pull it in.
        assert!(!selected.contains("harnx-serve"), "{selected:?}");
    }

    #[test]
    fn embedded_file_marks_the_embedding_crate_changed() {
        let selected = packages(select_paths(&["crates/harnx/models.yaml"]));
        assert!(selected.contains("harnx"), "{selected:?}");
        assert!(selected.contains("harnx-client"), "{selected:?}");
    }

    #[test]
    fn agent_package_change_selects_the_tests_that_read_it() {
        let selected = packages(select_paths(&["packages/pantheon/agents/zeus.md"]));
        assert_eq!(
            selected,
            BTreeSet::from(["harnx-bash-tools".to_owned(), "harnx-runtime".to_owned()])
        );
    }

    #[test]
    fn docs_only_change_selects_nothing() {
        assert_eq!(
            select_paths(&[
                "docs/healthz.md",
                ".changeset/example.md",
                "web/src/App.tsx"
            ]),
            Selection::Packages(BTreeSet::new())
        );
    }

    #[test]
    fn unowned_path_selects_the_whole_workspace() {
        assert_eq!(
            select_paths(&["scripts/new-script.sh"]),
            Selection::Workspace
        );
        assert_eq!(
            select_paths(&[".config/nextest.toml"]),
            Selection::Workspace
        );
        assert_eq!(select_paths(&[".gitattributes"]), Selection::Workspace);
    }

    #[test]
    fn cargo_args_name_each_package() {
        let selection = Selection::Packages(BTreeSet::from([
            "harnx".to_owned(),
            "harnx-core".to_owned(),
        ]));
        assert_eq!(selection.cargo_args(), ["-p", "harnx", "-p", "harnx-core"]);
        assert_eq!(Selection::Workspace.cargo_args(), ["--workspace"]);
        assert!(Selection::Packages(BTreeSet::new()).cargo_args().is_empty());
    }
}
