use harnx_toolset::{HARNX_SERVER_CONFIG, HARNX_SERVER_PACKAGE};

/// Package/config identity attached to a NATS toolset registration.
///
/// Standalone tool servers normally obtain this from the environment. In-process
/// servers can supply it explicitly so several differently-namespaced toolsets
/// can share one process without mutating process-wide environment variables.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RegistrationIdentity {
    /// Package namespace containing the tool server, if any.
    pub package: Option<String>,
    /// Tool-server configuration name, or an empty string when not configured.
    pub config: String,
}

impl RegistrationIdentity {
    /// Build a registration identity, treating an empty package as unscoped.
    pub fn new(package: Option<String>, config: impl Into<String>) -> Self {
        Self {
            package: package.filter(|package| !package.is_empty()),
            config: config.into(),
        }
    }

    pub(crate) fn from_env() -> Self {
        Self {
            package: std::env::var(HARNX_SERVER_PACKAGE)
                .ok()
                .filter(|package| !package.is_empty()),
            config: std::env::var(HARNX_SERVER_CONFIG).unwrap_or_default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvRestore {
        package: Option<OsString>,
        config: Option<OsString>,
    }

    impl EnvRestore {
        fn capture() -> Self {
            Self {
                package: std::env::var_os(HARNX_SERVER_PACKAGE),
                config: std::env::var_os(HARNX_SERVER_CONFIG),
            }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            set_env_var(HARNX_SERVER_PACKAGE, self.package.as_deref());
            set_env_var(HARNX_SERVER_CONFIG, self.config.as_deref());
        }
    }

    fn set_env_var(key: &str, value: Option<&std::ffi::OsStr>) {
        // SAFETY: these tests serialize access and restore both variables before
        // releasing the lock; production code only reads them.
        unsafe {
            if let Some(value) = value {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }
    }

    #[test]
    fn explicit_identity_normalizes_empty_package() {
        assert_eq!(
            RegistrationIdentity::new(Some(String::new()), "agent"),
            RegistrationIdentity::new(None, "agent")
        );
    }

    #[test]
    fn environment_identity_handles_set_empty_and_unset_package() {
        let _lock = ENV_LOCK.lock().expect("lock registration environment");
        let _restore = EnvRestore::capture();

        set_env_var(HARNX_SERVER_PACKAGE, Some(std::ffi::OsStr::new("pantheon")));
        set_env_var(HARNX_SERVER_CONFIG, Some(std::ffi::OsStr::new("agent")));
        assert_eq!(
            RegistrationIdentity::from_env(),
            RegistrationIdentity::new(Some("pantheon".to_string()), "agent")
        );

        set_env_var(HARNX_SERVER_PACKAGE, Some(std::ffi::OsStr::new("")));
        set_env_var(HARNX_SERVER_CONFIG, None);
        assert_eq!(
            RegistrationIdentity::from_env(),
            RegistrationIdentity::default()
        );

        set_env_var(HARNX_SERVER_PACKAGE, None);
        set_env_var(HARNX_SERVER_CONFIG, Some(std::ffi::OsStr::new("agent")));
        assert_eq!(
            RegistrationIdentity::from_env(),
            RegistrationIdentity::new(None, "agent")
        );
    }
}
