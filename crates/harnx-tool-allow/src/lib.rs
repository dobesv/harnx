mod allowlist;
mod batches;
#[cfg(unix)]
mod home_guard;
mod path_expand;
mod resolver;
#[cfg(unix)]
mod root_detection;
#[cfg(all(test, unix))]
mod test_support;
mod validation;

pub use allowlist::ResolvedAllowlist;
pub use batches::{all, common_default, dev_tools, repo_work, AllowEnv, AllowRule, Permission};
#[cfg(unix)]
pub use home_guard::{is_home_or_ancestor, resolve_path};
pub use path_expand::{expand_path_var, expand_tilde};
pub use resolver::{resolve_allowlist, AllowInputs};
#[cfg(unix)]
pub use root_detection::{detect_project_root, RootKind};
pub use validation::{
    default_root_from_cwd, path_is_home_or_ancestor, validate_path, validate_write_path,
};
