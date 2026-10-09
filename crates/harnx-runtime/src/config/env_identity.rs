//! Environment overrides for session identity and HTTP identity sources.

use super::*;

impl Config {
    pub(super) fn load_user_identity_envs(&mut self) {
        if let Some(v) = read_env_value::<String>(&get_env_name("user_id")) {
            self.user_id = v;
        }
        for (key, values) in [
            (
                "serve_user_id_sources",
                &mut self.data.serve_user_id_sources,
            ),
            ("serve_group_headers", &mut self.data.serve_group_headers),
            ("serve_role_headers", &mut self.data.serve_role_headers),
        ] {
            if let Ok(v) = env::var(get_env_name(key)) {
                *values = v
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_support::{env_lock, EnvGuard};

    #[test]
    fn serve_membership_headers_env_overrides_yaml_independently_and_can_clear() {
        harnx_core::require_nextest();
        let _lock = env_lock();
        let _groups = EnvGuard::remove("HARNX_SERVE_GROUP_HEADERS");
        let _roles = EnvGuard::remove("HARNX_SERVE_ROLE_HEADERS");
        let _users = EnvGuard::remove("HARNX_SERVE_USER_ID_SOURCES");
        let mut config = Config {
            data: serde_yaml::from_str(
                "serve_group_headers: [x-yaml-groups]\nserve_role_headers: [x-yaml-roles]\nserve_user_id_sources: [cookie:owner]\n",
            ).unwrap(),
            ..Default::default()
        };
        config.load_serve_envs();
        assert_eq!(config.serve_group_headers, ["x-yaml-groups"]);
        assert_eq!(config.serve_role_headers, ["x-yaml-roles"]);
        {
            let _groups = EnvGuard::new(
                "HARNX_SERVE_GROUP_HEADERS",
                " X-Groups, ,x-other-groups, X-Groups, ",
            );
            config.load_serve_envs();
            assert_eq!(
                config.serve_group_headers,
                ["X-Groups", "x-other-groups", "X-Groups"]
            );
            assert_eq!(config.serve_role_headers, ["x-yaml-roles"]);
            let _roles = EnvGuard::new("HARNX_SERVE_ROLE_HEADERS", " x-roles , , X-Other-Roles ,");
            config.load_envs(false).unwrap();
            assert_eq!(config.serve_role_headers, ["x-roles", "X-Other-Roles"]);
        }
        let _groups = EnvGuard::new("HARNX_SERVE_GROUP_HEADERS", "");
        let _roles = EnvGuard::new("HARNX_SERVE_ROLE_HEADERS", " , \t , ");
        config.load_serve_envs();
        assert!(config.serve_group_headers.is_empty());
        assert!(config.serve_role_headers.is_empty());
        assert_eq!(config.serve_user_id_sources, ["cookie:owner"]);
    }
    #[test]
    fn serve_user_id_sources_env_replaces_yaml_in_order_and_can_clear() {
        harnx_core::require_nextest();
        let _lock = env_lock();
        let _sources = EnvGuard::remove("HARNX_SERVE_USER_ID_SOURCES");
        let mut config = Config {
            data: serde_yaml::from_str("serve_user_id_sources: [cookie:yaml-owner]\n").unwrap(),
            ..Default::default()
        };
        config.load_serve_envs();
        assert_eq!(config.serve_user_id_sources, ["cookie:yaml-owner"]);
        {
            let _sources = EnvGuard::new(
                "HARNX_SERVE_USER_ID_SOURCES",
                " header:x-user, , cookie:owner ,x-backup,",
            );
            config.load_serve_envs();
            assert_eq!(
                config.serve_user_id_sources,
                ["header:x-user", "cookie:owner", "x-backup"]
            );
        }
        let _sources = EnvGuard::new("HARNX_SERVE_USER_ID_SOURCES", "");
        config.load_serve_envs();
        assert!(config.serve_user_id_sources.is_empty());
    }
    #[test]
    fn load_envs_user_id_overrides_yaml_and_can_clear_default() {
        harnx_core::require_nextest();
        let _lock = env_lock();
        let mut config = Config {
            data: serde_yaml::from_str("user_id: yaml-owner\n").unwrap(),
            ..Default::default()
        };
        {
            let _user_id = EnvGuard::new(get_env_name("user_id"), "env-owner");
            config.load_envs(false).unwrap();
            assert_eq!(config.user_id.as_deref(), Some("env-owner"));
        }
        {
            let _user_id = EnvGuard::new(get_env_name("user_id"), "null");
            config.load_envs(false).unwrap();
            assert_eq!(config.user_id, None);
        }
    }

    #[test]
    fn user_id_loads_from_dotenv_but_ambient_override_wins() {
        harnx_core::require_nextest();
        let _lock = env_lock();
        let _user_id = EnvGuard::remove("HARNX_USER_ID");
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join(".env");
        std::fs::write(&file, "HARNX_USER_ID=dotenv-owner\n").unwrap();
        let _env_file = EnvGuard::new("HARNX_ENV_FILE", &file);
        load_env_file().unwrap();
        let mut config = Config::default();
        config.load_envs(false).unwrap();
        assert_eq!(config.user_id.as_deref(), Some("dotenv-owner"));
        let _ambient = EnvGuard::new("HARNX_USER_ID", "ambient-owner");
        load_env_file().unwrap();
        config.load_envs(false).unwrap();
        assert_eq!(config.user_id.as_deref(), Some("ambient-owner"));
    }
}
