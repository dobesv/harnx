//! Loading optional user aliases from the config directory.
//!
//! Discovers `users.yaml` via `config_paths::local_path` once at startup.
//! Missing file returns `None`; present malformed files fail with path context.

use anyhow::{Context, Result};
use harnx_core::{config_paths::local_path, user_aliases::UserAliases};
use std::sync::Arc;

/// Load user aliases from the default config file, if present.
///
/// Uses default discovery through `config_paths::local_path("users.yaml")`.
/// Returns `Ok(None)` if the file does not exist (valid no-op).
/// Returns an error with path context for present but malformed files,
/// including permission denied and other I/O errors.
pub fn load_user_aliases() -> Result<Option<Arc<UserAliases>>> {
    let path = local_path("users.yaml");

    // Use try_exists to distinguish NotFound from other errors
    match path.try_exists() {
        Ok(true) => {
            // File exists, load it (will error on malformed content)
            UserAliases::load(&path).map(Arc::new).map(Some)
        }
        Ok(false) => {
            // File does not exist - valid no-op
            Ok(None)
        }
        Err(e) => {
            // I/O error (e.g., PermissionDenied) - fail with path context
            Err(e).with_context(|| {
                format!(
                    "failed to check existence of user aliases at {}",
                    path.display()
                )
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_environment::env_lock;

    const VALID_ALIASES: &str = r#"
- name: team-a
  identities: [alice, bob]
"#;

    fn write_aliases(path: &std::path::Path, yaml: &str) {
        std::fs::write(path, yaml).expect("write user aliases fixture");
    }

    #[test]
    fn aliases_load_from_default_file_when_present() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().expect("create fixture directory");
        std::env::set_var("HARNX_CONFIG_DIR", dir.path());
        write_aliases(&dir.path().join("users.yaml"), VALID_ALIASES);

        let aliases = load_user_aliases()
            .expect("load default aliases")
            .expect("default file is present");

        let identities: Vec<_> = aliases
            .expand_caller("alice")
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(identities.contains(&"alice".to_string()));
        assert!(identities.contains(&"bob".to_string()));
    }

    #[test]
    fn aliases_default_absent_returns_none() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().expect("create fixture directory");
        std::env::set_var("HARNX_CONFIG_DIR", dir.path());

        assert!(load_user_aliases()
            .expect("resolve absent default aliases")
            .is_none());
    }

    #[test]
    fn invalid_present_files_fail_with_path_context() {
        let _lock = env_lock();
        let cases = [
            ("empty document", ""),
            ("comment-only document", "# just a comment\n# nothing else"),
            ("whitespace-only document", "   \n  \n"),
            ("YAML parse error", "not a valid yaml: sequence: ["),
        ];

        for (case, contents) in cases {
            let dir = tempfile::tempdir().expect("create fixture directory");
            std::env::set_var("HARNX_CONFIG_DIR", dir.path());
            let path = dir.path().join("users.yaml");
            write_aliases(&path, contents);

            let error = load_user_aliases().unwrap_err();
            let err_str = format!("{error:#}");
            assert!(
                err_str.contains(&path.display().to_string()),
                "{case} should fail with path context, got: {err_str}"
            );
        }
    }
}
