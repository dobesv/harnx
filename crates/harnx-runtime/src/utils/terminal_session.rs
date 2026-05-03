//! Terminal session fingerprinting utility.
//!
//! Provides a unique identifier for the current terminal session by checking
//! various environment variables and system state.

use std::env;

/// Returns a unique identifier for the current terminal session, if determinable.
///
/// Checks environment variables in order:
/// 1. `TMUX_PANE` → `tmux:{val}`
/// 2. `STY` → `screen:{val}`
/// 3. `TERM_SESSION_ID` → `TERM_SESSION_ID:{val}`
/// 4. `WT_SESSION` → `WT_SESSION:{val}`
/// 5. `KITTY_WINDOW_ID` → `KITTY_WINDOW_ID:{val}`
///
/// On Linux, falls back to session ID + TTY + process start time:
/// `sid:{sid}:{tty}:{starttime}`
///
/// Returns `None` if no identifier could be determined.
///
/// This function never panics.
pub fn terminal_session_id() -> Option<String> {
    // Check environment variables in priority order
    // Multiplexers first (tmux, screen), then emulators
    if let Some(val) = env::var_os("TMUX_PANE") {
        if let Some(s) = val.to_str() {
            if !s.is_empty() {
                return Some(format!("tmux:{s}"));
            }
        }
    }

    if let Some(val) = env::var_os("STY") {
        if let Some(s) = val.to_str() {
            if !s.is_empty() {
                return Some(format!("screen:{s}"));
            }
        }
    }

    if let Some(val) = env::var_os("TERM_SESSION_ID") {
        if let Some(s) = val.to_str() {
            if !s.is_empty() {
                return Some(format!("TERM_SESSION_ID:{s}"));
            }
        }
    }

    if let Some(val) = env::var_os("WT_SESSION") {
        if let Some(s) = val.to_str() {
            if !s.is_empty() {
                return Some(format!("WT_SESSION:{s}"));
            }
        }
    }

    if let Some(val) = env::var_os("KITTY_WINDOW_ID") {
        if let Some(s) = val.to_str() {
            if !s.is_empty() {
                return Some(format!("KITTY_WINDOW_ID:{s}"));
            }
        }
    }

    // Linux fallback using getsid
    #[cfg(target_os = "linux")]
    {
        linux_fallback_session_id()
    }

    #[cfg(not(target_os = "linux"))]
    None
}

/// Linux-specific fallback using session ID, TTY, and process start time.
#[cfg(target_os = "linux")]
fn linux_fallback_session_id() -> Option<String> {
    // Get session ID
    let sid = get_linux_sid()?;

    // Get TTY
    let tty = read_tty_link().unwrap_or_else(|| "unknown".to_string());

    // Get start time from /proc/{sid}/stat
    let starttime = read_proc_starttime(sid).unwrap_or_else(|| "0".to_string());

    Some(format!("sid:{sid}:{tty}:{starttime}"))
}

#[cfg(target_os = "linux")]
fn get_linux_sid() -> Option<i32> {
    // Use libc getsid(0) to get the session ID of the calling process
    // Safe because getsid(0) just returns the session ID and doesn't modify memory
    let sid = unsafe { libc::getsid(0) };
    if sid < 0 {
        None
    } else {
        Some(sid)
    }
}

#[cfg(target_os = "linux")]
fn read_tty_link() -> Option<String> {
    use std::fs;
    use std::os::unix::fs::MetadataExt;

    // Try /proc/self/fd/0 (stdin)
    let tty_path = fs::read_link("/proc/self/fd/0").ok()?;
    let tty_str = tty_path.to_string_lossy().into_owned();

    // Check if it's actually a TTY by checking the mode
    let metadata = fs::metadata(&tty_path).ok()?;
    let mode = metadata.mode();

    // S_IFMT mask is 0o170000, S_IFCHR is 0o020000
    const S_IFMT: u32 = 0o170000;
    const S_IFCHR: u32 = 0o020000;

    if (mode & S_IFMT) == S_IFCHR {
        // Extract just the device name (e.g., "/dev/pts/0" -> "pts/0")
        let name = tty_str.strip_prefix("/dev/").unwrap_or(&tty_str);
        Some(name.to_string())
    } else {
        None
    }
}

#[cfg(target_os = "linux")]
fn read_proc_starttime(sid: i32) -> Option<String> {
    use std::fs;
    use std::io::Read;

    let path = format!("/proc/{sid}/stat");
    let mut file = fs::File::open(&path).ok()?;

    let mut contents = String::new();
    file.read_to_string(&mut contents).ok()?;

    // Format: pid (comm) state ppid pgrp session tty_nr tpgid flags ...
    // Field 22 is starttime (0-indexed: field 21)
    // But we need to handle the comm field which may contain spaces and parens

    // Find the last ')' to skip the comm field
    let comm_end = contents.rfind(')')?;
    let after_comm = &contents[comm_end + 1..];

    // Now fields are space-separated starting from field 3 (state)
    let fields: Vec<&str> = after_comm.split_whitespace().collect();

    // starttime is at index 18 (field 22 - 2 since we skipped pid and comm)
    // Fields after ): state ppid pgrp session tty_nr tpgid flags minflt cminflt majflt cmajflt utime stime cutime cstime priority nice num_threads itrealvalue starttime
    // Index:           0    1     2     3       4      5     6      7      8      9      10     11    12    13    14    15        16   17          18
    fields.get(18).map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Mutex to serialize tests that mutate process-global environment variables.
    static TEST_ENV_MUTEX: Mutex<()> = Mutex::new(());

    /// RAII guard that restores environment variables on drop.
    struct EnvRestore {
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl EnvRestore {
        fn new(keys: &[&'static str]) -> Self {
            let saved = keys.iter().map(|&k| (k, env::var_os(k))).collect();
            EnvRestore { saved }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            for (k, v) in &self.saved {
                match v {
                    Some(val) => env::set_var(k, val),
                    None => env::remove_var(k),
                }
            }
        }
    }

    #[test]
    fn test_with_term_session_id() {
        let _guard = TEST_ENV_MUTEX.lock().unwrap();
        let _restore = EnvRestore::new(&[
            "TERM_SESSION_ID",
            "WT_SESSION",
            "KITTY_WINDOW_ID",
            "TMUX_PANE",
            "STY",
        ]);

        env::set_var("TERM_SESSION_ID", "test123");

        // Clear other session vars to ensure TERM_SESSION_ID wins
        env::remove_var("WT_SESSION");
        env::remove_var("KITTY_WINDOW_ID");
        env::remove_var("TMUX_PANE");
        env::remove_var("STY");

        let result = terminal_session_id();
        assert_eq!(result, Some("TERM_SESSION_ID:test123".to_string()));
    }

    #[test]
    fn test_no_panic() {
        // This test ensures the function never panics
        // We can't easily test the fallback in a unit test environment,
        // but we can verify it doesn't panic
        let _ = terminal_session_id();
    }

    /// On Linux, when no terminal env vars are set, the fallback should
    /// return a `sid:...` identifier derived from getsid(0) and /proc.
    #[cfg(target_os = "linux")]
    #[test]
    fn test_linux_fallback_format() {
        let _guard = TEST_ENV_MUTEX.lock().unwrap();
        let _restore = EnvRestore::new(&[
            "TERM_SESSION_ID",
            "WT_SESSION",
            "KITTY_WINDOW_ID",
            "TMUX_PANE",
            "STY",
        ]);

        // Clear all terminal env vars so we fall through to the Linux fallback.
        for k in &[
            "TERM_SESSION_ID",
            "WT_SESSION",
            "KITTY_WINDOW_ID",
            "TMUX_PANE",
            "STY",
        ] {
            env::remove_var(k);
        }

        let result = linux_fallback_session_id();
        if let Some(id) = result {
            // Must be in format "sid:{integer}:{path}:{integer}"
            assert!(
                id.starts_with("sid:"),
                "Linux fallback must start with 'sid:'; got: {id}"
            );
            let parts: Vec<&str> = id.splitn(4, ':').collect();
            assert_eq!(parts.len(), 4, "Expected 4 colon-separated parts in '{id}'");
            assert!(
                parts[1].parse::<i32>().is_ok(),
                "Second part (SID) must be an integer in '{id}'"
            );
        }
        // If None — running without /proc access (e.g. container sandbox) — that's acceptable.
    }
}
