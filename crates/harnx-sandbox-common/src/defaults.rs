#[cfg(unix)]
use std::ffi::OsString;
#[cfg(unix)]
use std::path::{Path, PathBuf};

/// Linux `/run` audit for the least-privilege replacement of the
/// temporary blanket `/run` and `/var/run` exec entries below:
/// - `/run/systemd/resolve`: systemd-resolved resolv.conf symlink target.
/// - `/run/resolvconf`: resolvconf-generated resolv.conf on Debian/Ubuntu.
/// - `/run/NetworkManager`: NetworkManager-generated resolver files.
/// - `/run/current-system`: NixOS active system profile/wrapper symlink root.
/// - `/run/opengl-driver`: NixOS OpenGL/Mesa/Vulkan driver symlink farm.
/// - `/run/opengl-driver-32`: NixOS 32-bit OpenGL driver compatibility path.
/// - `/run/udev`: libudev/Mesa device metadata for GPU/device discovery.
///
/// `/run/user` is deliberately excluded: it contains the per-user DBus
/// session bus (`/run/user/<uid>/bus`) used by Secret Service/keyrings.
/// `/run/dbus` is deliberately excluded: it exposes the host system bus.
/// Modern Linux makes `/var/run` a symlink to `/run`; allow canonical
/// `/run/...` subpaths instead of re-adding top-level `/var/run`.
#[cfg(target_os = "linux")]
pub const SYSTEM_EXEC_PATHS: &[&str] = &[
    "/usr/bin",
    "/bin",
    "/usr/local/bin",
    "/usr/sbin",
    "/sbin",
    "/usr/lib",
    "/usr/lib64",
    "/lib",
    "/lib64",
    "/usr/lib/x86_64-linux-gnu",
    "/usr/libexec",
    "/proc",
    "/dev",
    "/sys",
    "/etc",
    "/tmp",
    "/run/systemd/resolve",
    "/run/resolvconf",
    "/run/NetworkManager",
    "/run/current-system",
    "/run/opengl-driver",
    "/run/opengl-driver-32",
    "/run/udev",
    "/usr/share",
];
#[cfg(target_os = "macos")]
pub const SYSTEM_EXEC_PATHS: &[&str] = &[
    "/usr/bin",
    "/bin",
    "/usr/local/bin",
    "/usr/sbin",
    "/sbin",
    "/usr/lib",
    "/usr/local/lib",
    "/Library",
    "/System",
    "/private/tmp",
    "/private/var",
    "/dev",
];
#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
pub const SYSTEM_EXEC_PATHS: &[&str] = &["/usr/bin", "/bin", "/tmp", "/etc"];

#[cfg(target_os = "linux")]
pub const SYSTEM_READ_PATHS: &[&str] = &["/usr/include", "/usr/include/x86_64-linux-gnu"];
#[cfg(target_os = "macos")]
pub const SYSTEM_READ_PATHS: &[&str] = &[];
#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
pub const SYSTEM_READ_PATHS: &[&str] = &["/usr/include"];

#[cfg(unix)]
pub const HOME_READ_PATHS: &[&str] = &[
    ".gitconfig",
    ".gitignore",
    ".gitignore_global",
    ".tool-versions",
];

#[cfg(unix)]
// Security hardening: writable + executable directories on PATH, or directories
// that hold host-executed binaries, let sandboxed code plant trojans that user
// may later run on host. Default to exec-only or write-only instead; users opt
// into write with --extra-rwx/--extra-write for installs and self-updates.
pub const HOME_EXEC_PATHS: &[&str] = &[
    ".local/bin",
    ".local/lib",
    ".bun",
    ".asdf",
    "go/bin",
    ".cargo",
    ".nvm",
    ".cargo/bin",
    ".mono",
    ".pyenv",
    ".rye",
    // AI coding agents whose self-updaters install versioned binaries under
    // ~/.local/share/<name>/ and symlink them from ~/.local/bin/:
    ".local/share/claude",   // Claude Code (`claude update`)
    ".local/share/opencode", // OpenCode
    // Python/JS package managers that install executables here:
    ".local/share/pipx", // pipx (installs aider, etc.)
];

#[cfg(unix)]
pub const HOME_WRITE_PATHS: &[&str] = &[
    ".cache",
    "go/pkg",
    ".npm",
    ".yarn",
    ".cargo/registry",
    ".cargo/git",
    ".bun/install/cache",
    ".local/share/pnpm", // pnpm store
    ".local/share/uv",   // uv (Python package manager)
];

#[cfg(unix)]
pub const HOME_RWX_PATHS: &[&str] = &[];

#[cfg(unix)]
pub fn push_home_relative_defaults(args: &mut Vec<OsString>, home: &Path) {
    for sub in HOME_READ_PATHS {
        args.push(OsString::from("--read"));
        args.push(home.join(sub).into_os_string());
    }
    for sub in HOME_EXEC_PATHS {
        args.push(OsString::from("--exec"));
        args.push(home.join(sub).into_os_string());
    }
    for sub in HOME_WRITE_PATHS {
        let path = home.join(sub);
        args.push(OsString::from("--read"));
        args.push(path.clone().into_os_string());
        args.push(OsString::from("--write"));
        args.push(path.into_os_string());
    }
    for sub in HOME_RWX_PATHS {
        let path = home.join(sub);
        args.push(OsString::from("--read"));
        args.push(path.clone().into_os_string());
        args.push(OsString::from("--write"));
        args.push(path.clone().into_os_string());
        args.push(OsString::from("--exec"));
        args.push(path.into_os_string());
    }
}

/// Honour toolchain-locating environment variables so users with non-default
/// install locations don't have to set extra sandbox overrides themselves.
#[cfg(unix)]
pub fn push_env_relative_defaults(args: &mut Vec<OsString>) {
    if let Some(cargo_home) = std::env::var_os("CARGO_HOME") {
        let cargo_home = PathBuf::from(cargo_home);
        // Root: read-only so config.toml / credentials are visible, matching the
        // default ~/.cargo (which is in HOME_EXEC_PATHS and thus readable).
        args.push(OsString::from("--read"));
        args.push(cargo_home.clone().into_os_string());
        // Binaries: read+exec only (same as the default ~/.cargo/bin).
        args.push(OsString::from("--exec"));
        args.push(cargo_home.join("bin").into_os_string());
        // Download caches: read+write (mirror the default ~/.cargo/registry and
        // ~/.cargo/git so a custom CARGO_HOME keeps the same cache-write access).
        for sub in ["registry", "git"] {
            let path = cargo_home.join(sub);
            args.push(OsString::from("--read"));
            args.push(path.clone().into_os_string());
            args.push(OsString::from("--write"));
            args.push(path.into_os_string());
        }
    }
    if let Some(goroot) = std::env::var_os("GOROOT") {
        args.push(OsString::from("--exec"));
        args.push(PathBuf::from(goroot).into_os_string());
    }
    if let Some(gopath) = std::env::var_os("GOPATH") {
        let gopath = PathBuf::from(gopath);
        args.push(OsString::from("--exec"));
        args.push(gopath.join("bin").into_os_string());
        let pkg = gopath.join("pkg");
        args.push(OsString::from("--read"));
        args.push(pkg.clone().into_os_string());
        args.push(OsString::from("--write"));
        args.push(pkg.into_os_string());
    }
    if let Some(gobin) = std::env::var_os("GOBIN") {
        args.push(OsString::from("--exec"));
        args.push(PathBuf::from(gobin).into_os_string());
    }
    if let Some(gomodcache) = std::env::var_os("GOMODCACHE") {
        let gomodcache = PathBuf::from(gomodcache);
        // Go caches hold source, .a archives, and build logs only; test binaries
        // link and execute from $TMPDIR instead. Granting exec here would be a
        // security regression.
        args.push(OsString::from("--read"));
        args.push(gomodcache.clone().into_os_string());
        args.push(OsString::from("--write"));
        args.push(gomodcache.into_os_string());
    }
    if let Some(gocache) = std::env::var_os("GOCACHE") {
        let gocache = PathBuf::from(gocache);
        // Go caches hold source, .a archives, and build logs only; test binaries
        // link and execute from $TMPDIR instead. Granting exec here would be a
        // security regression.
        args.push(OsString::from("--read"));
        args.push(gocache.clone().into_os_string());
        args.push(OsString::from("--write"));
        args.push(gocache.into_os_string());
    }
}

#[cfg(unix)]
pub fn system_writable_paths() -> Vec<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        vec![PathBuf::from("/tmp"), PathBuf::from("/dev/shm")]
    }
    #[cfg(target_os = "macos")]
    {
        let mut paths = vec![PathBuf::from("/private/tmp")];
        if let Ok(tmpdir) = std::env::var("TMPDIR") {
            let path = PathBuf::from(&tmpdir);
            if path != Path::new("/private/tmp") {
                paths.push(path);
            }
        }
        paths
    }
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    {
        vec![PathBuf::from("/tmp")]
    }
}
