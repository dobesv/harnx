//! macOS sandbox implementation that bypasses birdcage on this platform.
//!
//! The Seatbelt profile mirrors birdcage 0.8.1 with one addition: `(allow
//! file-ioctl)` in the default header so child processes can call `tcsetattr`
//! on their TTY. Without that operation, every TUI inside the sandbox
//! (claude/Ink, bash readline) silently loses raw mode and arrow keys leak as
//! literal `^[[A` etc.
//!
//! Behaviour matches birdcage **as observed through the `harnx-sandbox-exec`
//! CLI**, not method-for-method: `add_path` silently skips non-existent paths
//! (birdcage 0.8.1's `update_path_exceptions` errors via `canonicalize`),
//! preserving the existence-check the linux helper already had. Net effect for
//! the binary is identical.
//!
//! Birdcage's `Exception` API can only grant path/env/network access — there's
//! no public surface for adding profile-level operation exceptions like
//! `file-ioctl`, so this module reimplements the macOS-side profile builder
//! and `sandbox_init` call directly. Linux continues to use birdcage.
//!
//! This module is exposed `pub` so the `harnx-sandbox-exec` binary and the
//! integration test in `tests/macos_tty.rs` can reach it; it is not part of
//! the stable `harnx-sandbox-common` public API and may change without notice.

// Hidden from rustdoc because this is an internal helper, not part of the
// crate's public API surface. Downstream crates should call `harnx-sandbox-exec`
// rather than use `MacSandbox` directly.
#![doc(hidden)]

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::fs;
use std::os::raw::c_char;
use std::path::Path;
use std::process::{Child, Command};
use std::ptr;

use bitflags::bitflags;

const DEFAULT_PROFILE: &str = "\
(version 1)
(import \"system.sb\")

(deny default)
(allow mach*)
(allow ipc*)
(allow signal (target others))
(allow process-fork)
(allow sysctl*)
(allow system*)
(allow file-read-metadata)
(allow file-ioctl)
";

extern "C" {
    fn sandbox_init(profile: *const c_char, flags: u64, errorbuf: *mut *mut c_char) -> i32;
    fn sandbox_free_error(errorbuf: *mut c_char);
}

bitflags! {
    struct PathAccess: u8 {
        const EXEC  = 0b001;
        const WRITE = 0b010;
        const READ  = 0b100;
    }
}

#[derive(Default)]
pub struct MacSandbox {
    paths: HashMap<String, PathAccess>,
    env_exceptions: Vec<String>,
    full_env: bool,
    networking: bool,
}

impl MacSandbox {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn allow_read(&mut self, path: &Path) -> Result<(), String> {
        self.add_path(path, PathAccess::READ)
    }

    pub fn allow_write_and_read(&mut self, path: &Path) -> Result<(), String> {
        self.add_path(path, PathAccess::READ | PathAccess::WRITE)
    }

    pub fn allow_execute_and_read(&mut self, path: &Path) -> Result<(), String> {
        self.add_path(path, PathAccess::READ | PathAccess::EXEC)
    }

    pub fn allow_networking(&mut self) {
        self.networking = true;
    }

    pub fn allow_env(&mut self, var: String) {
        self.env_exceptions.push(var);
    }

    pub fn allow_full_env(&mut self) {
        self.full_env = true;
    }

    /// Record `access` for `path` in the sandbox profile.
    ///
    /// **Silently returns `Ok(())` when `path` does not exist** — the rule is
    /// dropped from the profile rather than erroring. This matches the
    /// `add_path_exception` wrapper the linux helper had before this rewrite
    /// (which dates to issue #619), so the `harnx-sandbox-exec` CLI surface
    /// is unchanged. Diverges from birdcage 0.8.1's `update_path_exceptions`,
    /// which surfaces a `canonicalize` error for missing paths.
    fn add_path(&mut self, path: &Path, access: PathAccess) -> Result<(), String> {
        if !path.exists() {
            return Ok(());
        }
        let escaped = escape_path(path)?;
        self.paths
            .entry(escaped)
            .or_insert(PathAccess::empty())
            .insert(access);
        Ok(())
    }

    /// Apply the Seatbelt profile to the current process, then spawn `command`.
    /// The child inherits the sandbox via fork.
    ///
    /// **Caller threading constraint:** must be called from a single-threaded
    /// process or from a position where no other thread can be reading the
    /// environment. The implementation calls `std::env::remove_var`, which is
    /// unsound when other threads are concurrently reading env. The
    /// `harnx-sandbox-exec` binary satisfies this trivially (no async runtime,
    /// no extra threads spawned before this call); external callers must
    /// either uphold the same invariant or use `allow_full_env()` to skip env
    /// restriction entirely.
    pub fn apply_and_spawn(self, mut command: Command) -> Result<Child, String> {
        // Build + install the Seatbelt profile FIRST so a sandbox_init failure
        // returns Err without having mutated the parent process's env.
        let profile = self.build_profile();
        let c_profile =
            CString::new(profile).map_err(|_| "sandbox profile contained NUL byte".to_string())?;
        let mut err: *mut c_char = ptr::null_mut();
        // SAFETY: `sandbox_init` is the documented Seatbelt entry point. We
        // pass a NUL-terminated profile and a writable pointer to receive the
        // error string (which we free with `sandbox_free_error`).
        let rc = unsafe { sandbox_init(c_profile.as_ptr(), 0, &mut err) };
        if rc != 0 {
            let msg = unsafe { CStr::from_ptr(err) }
                .to_string_lossy()
                .into_owned();
            unsafe { sandbox_free_error(err) };
            return Err(format!("sandbox_init failed: {msg}"));
        }

        // With the sandbox now installed, strip non-allowlisted env vars from
        // the current process so the forked child doesn't inherit them.
        // Mirrors birdcage's `restrict_env_variables`. SAFETY for
        // `env::remove_var`: see the method docstring above — caller is
        // responsible for the single-threaded invariant.
        if !self.full_env {
            let keep: Vec<String> = self.env_exceptions.clone();
            for (key, _) in std::env::vars() {
                if !keep.iter().any(|k| k == &key) {
                    unsafe { std::env::remove_var(&key) };
                }
            }
        }

        command.spawn().map_err(|e| format!("spawn failed: {e}"))
    }

    fn build_profile(&self) -> String {
        let mut p = DEFAULT_PROFILE.to_string();

        // Sort parents before children so a more-restrictive child can
        // override a granted parent. Matches birdcage 0.8.1 ordering.
        let mut paths: Vec<_> = self.paths.iter().collect();
        paths.sort_by_key(|(s, _)| s.len());

        for (path, access) in paths {
            // Clear any inherited grants for this exact subpath first.
            for op in ["file-read*", "file-write*", "process-exec"] {
                p.push_str(&format!("(deny {op} (subpath {path}))\n"));
            }
            if access.contains(PathAccess::READ) {
                p.push_str(&format!("(allow file-read* (subpath {path}))\n"));
            }
            if access.contains(PathAccess::WRITE) {
                p.push_str(&format!("(allow file-write* (subpath {path}))\n"));
            }
            if access.contains(PathAccess::EXEC) {
                p.push_str(&format!("(allow process-exec (subpath {path}))\n"));
            }
        }

        if self.networking {
            p.push_str("(allow network*)\n");
        }
        p.push_str("(system-network)\n");
        p
    }
}

/// Canonicalize and quote a path for use in a Seatbelt `subpath` expression.
///
/// Escapes backslashes BEFORE quotes — the reverse order (which birdcage 0.8.1
/// has) double-escapes the backslash inserted by the quote-escape pass,
/// producing malformed SBPL for paths containing `"`. macOS paths can legally
/// contain both characters, so the order matters for correctness.
fn escape_path(path: &Path) -> Result<String, String> {
    let canonical =
        fs::canonicalize(path).map_err(|_| format!("invalid path: {}", path.display()))?;
    let mut s = canonical
        .into_os_string()
        .into_string()
        .map_err(|_| format!("path not valid UTF-8: {}", path.display()))?;
    while s.ends_with('/') && s != "/" {
        s.pop();
    }
    let s = s.replace('\\', "\\\\").replace('"', "\\\"");
    Ok(format!("\"{s}\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_profile_allows_file_ioctl() {
        let s = MacSandbox::new();
        let p = s.build_profile();
        assert!(
            p.contains("(allow file-ioctl)"),
            "default profile must allow file-ioctl so tcsetattr works inside the sandbox"
        );
    }

    #[test]
    fn read_path_emits_subpath_rule() {
        let dir = std::env::temp_dir(); // always exists, gets canonicalized
        let canonical = std::fs::canonicalize(&dir).unwrap();
        let canonical_str = canonical.to_str().unwrap();

        let mut s = MacSandbox::new();
        s.allow_read(&dir).unwrap();
        let p = s.build_profile();
        let expected = format!("(allow file-read* (subpath \"{canonical_str}\"))");
        assert!(
            p.contains(&expected),
            "profile missing expected rule {expected}, got:\n{p}"
        );
    }

    #[test]
    fn networking_flag_appends_rule() {
        let mut s = MacSandbox::new();
        s.allow_networking();
        let p = s.build_profile();
        assert!(p.contains("(allow network*)"));
    }

    /// Helper: canonical path string for a directory that always exists.
    fn canonical_temp() -> String {
        let dir = std::env::temp_dir();
        std::fs::canonicalize(&dir)
            .unwrap()
            .into_os_string()
            .into_string()
            .unwrap()
    }

    #[test]
    fn write_path_emits_subpath_rule() {
        let canonical_str = canonical_temp();
        let mut s = MacSandbox::new();
        s.allow_write_and_read(Path::new(&canonical_str)).unwrap();
        let p = s.build_profile();
        assert!(
            p.contains(&format!(
                "(allow file-write* (subpath \"{canonical_str}\"))"
            )),
            "missing file-write* allow for {canonical_str} in:\n{p}"
        );
        assert!(
            p.contains(&format!("(allow file-read* (subpath \"{canonical_str}\"))")),
            "write_and_read should also emit file-read* allow for {canonical_str}"
        );
    }

    #[test]
    fn execute_path_emits_subpath_rule() {
        let canonical_str = canonical_temp();
        let mut s = MacSandbox::new();
        s.allow_execute_and_read(Path::new(&canonical_str)).unwrap();
        let p = s.build_profile();
        assert!(
            p.contains(&format!(
                "(allow process-exec (subpath \"{canonical_str}\"))"
            )),
            "missing process-exec allow for {canonical_str} in:\n{p}"
        );
        assert!(
            p.contains(&format!("(allow file-read* (subpath \"{canonical_str}\"))")),
            "execute_and_read should also emit file-read* allow for {canonical_str}"
        );
    }

    #[test]
    fn rwx_emits_all_three_allow_rules() {
        let canonical_str = canonical_temp();
        let p_ref = Path::new(&canonical_str);
        let mut s = MacSandbox::new();
        s.allow_read(p_ref).unwrap();
        s.allow_write_and_read(p_ref).unwrap();
        s.allow_execute_and_read(p_ref).unwrap();
        let p = s.build_profile();
        for op in ["file-read*", "file-write*", "process-exec"] {
            let expected = format!("(allow {op} (subpath \"{canonical_str}\"))");
            assert!(
                p.contains(&expected),
                "RWX path missing {op} allow rule, got profile:\n{p}"
            );
        }
    }

    #[test]
    fn path_rules_use_deny_then_allow_pattern() {
        // The macro pattern revokes inherited grants before adding our allow,
        // so a parent-granted access can be tightened on a child path. Each
        // path must emit `deny` lines *before* its `allow` lines.
        let canonical_str = canonical_temp();
        let mut s = MacSandbox::new();
        s.allow_read(Path::new(&canonical_str)).unwrap();
        let p = s.build_profile();
        let deny = format!("(deny file-read* (subpath \"{canonical_str}\"))");
        let allow = format!("(allow file-read* (subpath \"{canonical_str}\"))");
        let deny_pos = p
            .find(&deny)
            .unwrap_or_else(|| panic!("deny line missing in:\n{p}"));
        let allow_pos = p
            .find(&allow)
            .unwrap_or_else(|| panic!("allow line missing in:\n{p}"));
        assert!(
            deny_pos < allow_pos,
            "deny must precede allow so per-path overrides survive parent grants"
        );
    }

    #[test]
    fn parents_sort_before_children() {
        // Per-test temp dir keeps parallel `cargo nextest` runs from racing on
        // a shared path; the TempDir guard removes the whole tree on drop so
        // we don't depend on `remove_dir` succeeding against a non-empty dir.
        let parent_guard = tempfile::tempdir().expect("tempdir");
        let parent = std::fs::canonicalize(parent_guard.path()).unwrap();
        let child = parent.join("child");
        std::fs::create_dir(&child).unwrap();

        let mut s = MacSandbox::new();
        // Add in reversed order — sort should put parent first.
        s.allow_read(&child).unwrap();
        s.allow_read(&parent).unwrap();
        let p = s.build_profile();

        let parent_str = parent.to_str().unwrap();
        let child_str = child.to_str().unwrap();
        let parent_allow = format!("(allow file-read* (subpath \"{parent_str}\"))");
        let child_allow = format!("(allow file-read* (subpath \"{child_str}\"))");
        let parent_pos = p.find(&parent_allow).expect("parent allow line missing");
        let child_pos = p.find(&child_allow).expect("child allow line missing");
        assert!(
            parent_pos < child_pos,
            "parent path must emit before child path so child rules can override"
        );
    }

    #[test]
    fn nonexistent_path_silently_skipped() {
        // Regression guard for the `if !path.exists()` early-return in
        // `add_path`. Mirrors the linux-side test in `sandbox_exec.rs`
        // (`test_write_exception_nonexistent_path`) so the same invariant is
        // pinned on macOS.
        let dir_guard = tempfile::tempdir().expect("tempdir");
        let missing = dir_guard.path().join("does-not-exist");
        assert!(!missing.exists(), "test setup: path must not exist");

        let mut s = MacSandbox::new();
        s.allow_read(&missing)
            .expect("non-existent path should be Ok");
        s.allow_write_and_read(&missing)
            .expect("non-existent path should be Ok");
        s.allow_execute_and_read(&missing)
            .expect("non-existent path should be Ok");

        let p = s.build_profile();
        assert!(
            !p.contains("does-not-exist"),
            "non-existent path must not emit any rule, got profile:\n{p}"
        );
    }

    #[test]
    fn escape_path_handles_embedded_quote() {
        // A path containing `"` must produce SBPL that parses correctly.
        // Bug-history: if backslash were escaped AFTER quote, the `\` inserted
        // by the quote-escape pass would itself be doubled, yielding `\\"` —
        // an escaped backslash followed by an unescaped quote that
        // prematurely terminates the subpath string.
        let dir_guard = tempfile::tempdir().expect("tempdir");
        let weird = dir_guard.path().join("foo\"bar");
        std::fs::create_dir(&weird).unwrap();
        let canonical = std::fs::canonicalize(&weird).unwrap();
        let canonical_str = canonical.to_str().unwrap();
        // What we expect after correct escaping: each `"` becomes `\"`,
        // no extra backslashes injected.
        let expected_escaped = canonical_str.replace('"', "\\\"");

        let mut s = MacSandbox::new();
        s.allow_read(&weird).unwrap();
        let p = s.build_profile();
        let expected = format!("(allow file-read* (subpath \"{expected_escaped}\"))");
        assert!(
            p.contains(&expected),
            "embedded `\"` not escaped correctly. expected:\n  {expected}\ngot profile:\n{p}"
        );
    }
}
