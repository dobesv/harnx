//! Loading optional access rules from an explicit path or the config directory.

use anyhow::Result;
use harnx_core::{access_rules::AccessRules, config_paths::local_path};
use std::{path::PathBuf, sync::Arc};

/// Load access rules from an explicit path or the default config file, if present.
///
/// Callers should pass the CLI option value, including values populated by
/// clap's `HARNX_ACCESS_RULES` environment binding, as `explicit`.
pub fn load_access_rules(explicit: Option<PathBuf>) -> Result<Option<Arc<AccessRules>>> {
    let path = match explicit {
        Some(path) => path,
        None => {
            let path = local_path("access.yaml");
            if !path.exists() {
                return Ok(None);
            }
            path
        }
    };

    AccessRules::load(&path).map(Arc::new).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_environment::{env_lock, EnvGuard};
    use std::{fs, path::Path};
    use tempfile::TempDir;

    const VALID_ACCESS_RULES: &str = "rules:\n  - agents: [sisyphus]\n    users: [alice]\n";

    fn private_config_dir() -> (TempDir, EnvGuard) {
        let dir = tempfile::tempdir().expect("create private config directory");
        let guard = EnvGuard::new("HARNX_CONFIG_DIR", dir.path());
        (dir, guard)
    }

    fn write_rules(path: &Path, yaml: &str) {
        fs::write(path, yaml).expect("write access rules fixture");
    }

    #[test]
    fn access_rules_load_from_explicit_path() {
        let dir = tempfile::tempdir().expect("create fixture directory");
        let path = dir.path().join("custom-access.yaml");
        write_rules(&path, VALID_ACCESS_RULES);

        let rules = load_access_rules(Some(path)).expect("load explicit access rules");
        assert!(rules.is_some());
        assert!(rules
            .as_ref()
            .unwrap()
            .can_see_agent("sisyphus", &["alice"]));
    }

    #[test]
    fn access_rules_load_from_cli_env_path() {
        // clap resolves HARNX_ACCESS_RULES into the explicit CLI argument before
        // invoking this helper; exercise that same path without changing process env.
        let dir = tempfile::tempdir().expect("create fixture directory");
        let env_path = dir.path().join("env-access.yaml");
        write_rules(&env_path, VALID_ACCESS_RULES);

        let rules =
            load_access_rules(Some(env_path)).expect("load rules from env-provided CLI path");
        assert!(rules.is_some());
    }

    #[test]
    fn access_rules_load_default_file_when_present() {
        let _lock = env_lock();
        let (dir, _guard) = private_config_dir();
        write_rules(&dir.path().join("access.yaml"), VALID_ACCESS_RULES);

        let rules = load_access_rules(None)
            .expect("load default access rules")
            .expect("default file is present");
        assert!(rules.can_see_agent("sisyphus", &["alice"]));
    }

    #[test]
    fn access_rules_default_absent_returns_none() {
        let _lock = env_lock();
        let (_dir, _guard) = private_config_dir();

        assert!(load_access_rules(None)
            .expect("resolve absent default access rules")
            .is_none());
    }

    #[test]
    fn access_rules_explicit_missing_path_is_an_error() {
        let dir = tempfile::tempdir().expect("create fixture directory");
        let path = dir.path().join("missing-access.yaml");

        let error = load_access_rules(Some(path.clone())).unwrap_err();
        assert!(error.to_string().contains(&path.display().to_string()));
    }

    #[test]
    fn access_rules_parse_error_mentions_path() {
        let dir = tempfile::tempdir().expect("create fixture directory");
        let path = dir.path().join("invalid-access.yaml");
        write_rules(&path, "rules: [not valid");

        let error = load_access_rules(Some(path.clone())).unwrap_err();
        assert!(format!("{error:#}").contains(&path.display().to_string()));
    }
}
