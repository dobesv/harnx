//! Shared domain types, event model, and pure utilities used across the
//! harnx workspace. See the spec at
//! `docs/superpowers/specs/2026-04-19-monorepo-refactor-design.md` for the
//! role this crate plays in the multi-crate split.

pub mod abort;
pub mod access_rules;
pub mod agent_config;
pub mod agent_ref;
pub mod alloc_guard;
pub mod api_types;
pub mod attachments;
pub mod child_process;
pub mod cid_url;
pub mod cli;
pub mod compaction_summary;
pub mod config_data;
pub mod config_paths;
pub mod context;
pub mod crypto;
pub mod error;
pub mod event;
pub mod execution_context;
pub mod hooks;
pub mod input;
pub mod instance;
pub mod json_schema;
pub mod last_message;
pub mod llm_trace;
pub mod logging;
pub mod loop_guard;
pub mod macros;
pub mod message;
pub mod model;
pub mod package;
pub mod package_namespace;
pub mod partial_result;
pub mod path;
pub mod provider_config;
pub mod retry_config;
pub mod runtime_note;
pub mod safety;
pub mod session;
pub mod session_identity;
pub mod session_log;
pub mod session_reconstruct;
pub mod sink;
pub mod system_vars;
pub mod text;
pub mod timing_identity;
pub mod tool;
pub mod transcript_view;
pub mod user_aliases;
pub mod working_mode;

pub mod jaq;

/// Assert that the current test is running under `cargo nextest`, panicking with
/// guidance otherwise.
///
/// Some tests rely on per-test process isolation because they mutate
/// process-global state (environment variables, the shared model registry, tmux
/// sessions, etc.). `cargo test` runs every test of a binary in a single process
/// with shared threads, which makes these tests flaky and produces confusing
/// failures. `cargo nextest` runs each test in its own process and is the
/// supported runner.
///
/// Nextest sets `NEXTEST=1` in every test process; `cargo test` does not. Call
/// this as the first line of a test that must not run under `cargo test`.
#[track_caller]
pub fn require_nextest() {
    if std::env::var_os("NEXTEST").is_none() {
        panic!(
            "\n\n\
             This test must be run with cargo-nextest, not `cargo test`.\n\
             It mutates process-global state and needs nextest's per-test process isolation;\n\
             `cargo test` shares one process across tests and produces spurious failures.\n\n\
             Run instead:\n\
             \tcargo nextest run --workspace\n\n\
             See AGENTS.md (\"Verifying Changes\") for the full verification pipeline.\n"
        );
    }
}

/// The directory that holds the workspace's binaries, found from the path of an
/// executable that runs out of the target directory (usually
/// `std::env::current_exe()`).
///
/// Workspace binaries are copied to `<target>/<profile>/`. Test executables
/// are not: before Cargo 1.100 they sat in `<profile>/deps/`, and since then
/// they sit in the build directory, at `<profile>/build/<package>/<hash>/out/`.
/// Looking for a sibling binary next to a test executable therefore has to
/// climb out of either place. A binary already in `<profile>/` is its own
/// answer.
pub fn bin_dir_of_exe(exe: &std::path::Path) -> std::path::PathBuf {
    let dir = exe.parent().unwrap_or(exe);
    let named = |path: &std::path::Path, name: &str| path.file_name().is_some_and(|n| n == name);
    if named(dir, "deps") {
        return dir.parent().unwrap_or(dir).to_path_buf();
    }
    if named(dir, "out") {
        // out, <hash>, <package>, build, <profile>
        let mut ancestors = dir.ancestors().skip(3);
        if let (Some(build), Some(profile)) = (ancestors.next(), ancestors.next()) {
            if named(build, "build") {
                return profile.to_path_buf();
            }
        }
    }
    dir.to_path_buf()
}

/// Whether this process is refused permission to create Unix domain sockets.
/// Claude Code's sandbox refuses them on Linux, through a seccomp filter its
/// `sandbox.network.allowAllUnixSockets` setting turns off. A test that needs
/// one, such as a tmux or llama-server test, skips when this is true. Any
/// other failure to create a socket is left for the test itself to report.
/// Always false off Unix.
pub fn unix_sockets_blocked() -> bool {
    #[cfg(unix)]
    {
        std::os::unix::net::UnixDatagram::unbound()
            .is_err_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied)
    }
    #[cfg(not(unix))]
    {
        false
    }
}

#[cfg(test)]
mod bin_dir_of_exe_tests {
    use super::bin_dir_of_exe;
    use std::path::Path;

    #[test]
    fn binary_in_the_profile_directory_is_its_own_answer() {
        let exe = Path::new("/w/target/debug/harnx");
        assert_eq!(bin_dir_of_exe(exe), Path::new("/w/target/debug"));
    }

    #[test]
    fn test_executable_in_deps_resolves_to_the_profile_directory() {
        let exe = Path::new("/w/target/debug/deps/it-0123abcd");
        assert_eq!(bin_dir_of_exe(exe), Path::new("/w/target/debug"));
    }

    #[test]
    fn test_executable_in_the_build_directory_resolves_to_the_profile_directory() {
        let exe = Path::new("/w/target/debug/build/harnx-runtime/5f9cb5a22f7d7d94/out/it-5f9cb5a2");
        assert_eq!(bin_dir_of_exe(exe), Path::new("/w/target/debug"));
        let cross = Path::new("/w/target/x86_64-pc-windows-gnu/release/build/p/h/out/it");
        assert_eq!(
            bin_dir_of_exe(cross),
            Path::new("/w/target/x86_64-pc-windows-gnu/release")
        );
    }

    #[test]
    fn an_out_directory_outside_the_build_layout_is_left_alone() {
        let exe = Path::new("/somewhere/out/tool");
        assert_eq!(bin_dir_of_exe(exe), Path::new("/somewhere/out"));
    }
}
