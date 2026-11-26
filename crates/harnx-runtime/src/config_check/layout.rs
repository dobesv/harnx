//! Which files a config directory holds, by kind, found the way the loaders
//! find them: the same directory names, and the same package order and alias
//! handling as `Config::load_from_file`.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use harnx_core::config_paths::{
    self, AGENTS_DIR_NAME, CLIENTS_DIR_NAME, CONFIG_FILE_NAME, MACROS_DIR_NAME,
    NATS_SERVERS_DIR_NAME, PACKAGES_DIR_NAME, PACKAGE_MANIFEST_FILE_NAME,
    PACKAGE_METADATA_FILE_NAME, SCRIPT_TOOLS_DIR_NAME, TOOL_SERVERS_DIR_NAME,
};

use crate::config::Config;

/// Package subdirectory of bash tool templates.
const BASH_TOOLS_DIR_NAME: &str = "bash_tools";
const MODELS_OVERRIDE_FILE_NAME: &str = "models-override.yaml";
const ACCESS_FILE_NAME: &str = "access.yaml";
const USERS_FILE_NAME: &str = "users.yaml";

/// Where each kind of file lives. harnx reads from two directories:
/// `root`, the config file's parent, holds `clients/`, `tool_servers/`,
/// `nats_servers/` and `agents/`; `home`, the config directory, holds
/// `packages/`, `models-override.yaml`, `access.yaml` and `users.yaml`.
/// They differ only when `HARNX_CONFIG_FILE` points outside the config
/// directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSources {
    /// The directory findings are reported relative to.
    pub root: PathBuf,
    pub config_file: PathBuf,
    pub home: PathBuf,
    pub macros_dir: PathBuf,
}

impl ConfigSources {
    /// Everything inside `dir`, ignoring harnx's environment variables.
    pub fn in_dir(dir: &Path) -> Self {
        Self {
            root: dir.to_path_buf(),
            config_file: dir.join(CONFIG_FILE_NAME),
            home: dir.to_path_buf(),
            macros_dir: dir.join(MACROS_DIR_NAME),
        }
    }

    /// The paths harnx itself would use, from `HARNX_CONFIG_DIR`,
    /// `HARNX_CONFIG_FILE`, `HARNX_MACROS_DIR` and the XDG defaults.
    pub fn from_env() -> Self {
        Self {
            root: config_paths::config_dir_path(),
            config_file: config_paths::config_file(),
            home: config_paths::config_dir(),
            macros_dir: config_paths::macros_dir(),
        }
    }

    pub fn packages_dir(&self) -> PathBuf {
        self.home.join(PACKAGES_DIR_NAME)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigLayout {
    pub root: PathBuf,
    pub config_file: Option<PathBuf>,
    pub clients: Vec<PathBuf>,
    pub tool_servers: Vec<PathBuf>,
    pub nats_servers: Vec<PathBuf>,
    /// Agent files directly under `agents/`; fragments in subdirectories such
    /// as `agents/shared/` are not agents.
    pub agents: Vec<PathBuf>,
    pub macros: Vec<PathBuf>,
    pub models_override: Option<PathBuf>,
    pub access: Option<PathBuf>,
    pub users: Option<PathBuf>,
    pub packages: Vec<PackageLayout>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackageLayout {
    pub name: String,
    /// `packages/<name>` under the root, as listed (not resolved through a
    /// symlink).
    pub dir: PathBuf,
    /// `packages/<name>.patch.yaml`.
    pub patch_file: Option<PathBuf>,
    pub clients: Vec<PathBuf>,
    pub tool_servers: Vec<PathBuf>,
    pub agents: Vec<PathBuf>,
    pub script_tools: Vec<PathBuf>,
    pub bash_tools: Vec<PathBuf>,
    pub manifest: Option<PathBuf>,
    pub metadata: Option<PathBuf>,
}

impl ConfigLayout {
    /// Nothing that configures harnx: no config file, agents, clients, tool
    /// servers or packages.
    pub fn is_empty(&self) -> bool {
        self.config_file.is_none()
            && self.agents.is_empty()
            && self.clients.is_empty()
            && self.tool_servers.is_empty()
            && self.packages.is_empty()
    }

    /// Every file the layout lists.
    pub fn files(&self) -> Vec<&Path> {
        let optional = [
            &self.config_file,
            &self.models_override,
            &self.access,
            &self.users,
        ];
        let lists = [
            &self.clients,
            &self.tool_servers,
            &self.nats_servers,
            &self.agents,
            &self.macros,
        ];
        let mut files: Vec<&Path> = optional
            .into_iter()
            .flatten()
            .map(PathBuf::as_path)
            .collect();
        files.extend(lists.into_iter().flatten().map(PathBuf::as_path));
        for package in &self.packages {
            files.extend(package.files());
        }
        files
    }
}

impl PackageLayout {
    fn discover(packages_dir: &Path, name: String, dir: PathBuf) -> Self {
        let patch_file = existing_file(packages_dir.join(format!("{name}.patch.yaml")));
        Self {
            patch_file,
            clients: files_with_extension(&dir.join(CLIENTS_DIR_NAME), "yaml"),
            tool_servers: files_with_extension(&dir.join(TOOL_SERVERS_DIR_NAME), "yaml"),
            agents: files_with_extension(&dir.join(AGENTS_DIR_NAME), "md"),
            script_tools: files_with_extension(&dir.join(SCRIPT_TOOLS_DIR_NAME), "yaml"),
            bash_tools: files_with_extension(&dir.join(BASH_TOOLS_DIR_NAME), "yaml"),
            manifest: existing_file(dir.join(PACKAGE_MANIFEST_FILE_NAME)),
            metadata: existing_file(dir.join(PACKAGE_METADATA_FILE_NAME)),
            name,
            dir,
        }
    }

    fn files(&self) -> impl Iterator<Item = &Path> {
        let optional = [&self.patch_file, &self.manifest, &self.metadata];
        let lists = [
            &self.clients,
            &self.tool_servers,
            &self.agents,
            &self.script_tools,
            &self.bash_tools,
        ];
        optional
            .into_iter()
            .flatten()
            .chain(lists.into_iter().flatten())
            .map(PathBuf::as_path)
    }
}

/// List the files in `root`, a config directory with no `HARNX_*`
/// redirection.
pub fn discover(root: &Path) -> Result<ConfigLayout> {
    discover_sources(&ConfigSources::in_dir(root))
}

/// List the files `sources` names. Fails when `sources.root` can't be read;
/// unreadable subdirectories list as empty, as the loaders treat them.
pub fn discover_sources(sources: &ConfigSources) -> Result<ConfigLayout> {
    ensure_readable_dir(&sources.root)?;
    let root = sources.root.as_path();
    let packages_dir = sources.packages_dir();
    let packages = Config::sorted_package_dirs(&packages_dir)?
        .into_iter()
        .map(|(name, dir)| PackageLayout::discover(&packages_dir, name, dir))
        .collect();
    Ok(ConfigLayout {
        root: root.to_path_buf(),
        config_file: existing_file(sources.config_file.clone()),
        clients: files_with_extension(&root.join(CLIENTS_DIR_NAME), "yaml"),
        tool_servers: files_with_extension(&root.join(TOOL_SERVERS_DIR_NAME), "yaml"),
        nats_servers: files_with_extension(&root.join(NATS_SERVERS_DIR_NAME), "yaml"),
        agents: files_with_extension(&root.join(AGENTS_DIR_NAME), "md"),
        macros: files_with_extension(&sources.macros_dir, "yaml"),
        models_override: existing_file(sources.home.join(MODELS_OVERRIDE_FILE_NAME)),
        access: existing_file(sources.home.join(ACCESS_FILE_NAME)),
        users: existing_file(sources.home.join(USERS_FILE_NAME)),
        packages,
    })
}

/// A directory the check can't list would otherwise look empty and pass.
fn ensure_readable_dir(dir: &Path) -> Result<()> {
    let metadata = match std::fs::metadata(dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!("Config directory '{}' does not exist", dir.display())
        }
        result => {
            result.with_context(|| format!("Cannot read config directory '{}'", dir.display()))?
        }
    };
    if !metadata.is_dir() {
        bail!("Config directory '{}' is not a directory", dir.display());
    }
    std::fs::read_dir(dir)
        .with_context(|| format!("Cannot read config directory '{}'", dir.display()))?;
    Ok(())
}

fn existing_file(path: PathBuf) -> Option<PathBuf> {
    path.is_file().then_some(path)
}

/// Regular files directly in `dir` with `extension`, sorted.
fn files_with_extension(dir: &Path, extension: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some(extension)
        })
        .collect();
    files.sort();
    files
}
