pub mod args;
pub mod config;
#[cfg(unix)]
pub mod defaults;
#[cfg(unix)]
pub mod home_guard;
pub mod path_expand;
#[cfg(unix)]
pub mod root_detection;
#[cfg(all(test, unix))]
pub(crate) mod test_support;

pub use args::build_default_sandbox_args;
pub use config::SandboxConfig;

#[cfg(unix)]
pub use defaults::{
    push_env_relative_defaults, push_home_relative_defaults, system_writable_paths,
    HOME_EXEC_PATHS, HOME_READ_PATHS, HOME_RWX_PATHS, HOME_WRITE_PATHS, SYSTEM_EXEC_PATHS,
    SYSTEM_READ_PATHS,
};
#[cfg(unix)]
pub use home_guard::{is_home_or_ancestor, resolve_path};
pub use path_expand::{expand_path_var, expand_tilde};
#[cfg(unix)]
pub use root_detection::{detect_project_root, RootKind};
