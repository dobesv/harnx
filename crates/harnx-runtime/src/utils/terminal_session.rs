//! Terminal session fingerprinting utility.
//!
//! Provides a unique identifier for the current terminal session by checking
//! various environment variables and system state.

use std::env;

/// Returns a unique identifier for the current terminal session, if determinable.
///
/// Checks environment variables in order:
/// 1. `TERM_SESSION_ID` → `TERM_SESSION_ID:{val}`
/// 2. `WT_SESSION` → `WT_SESSION:{val}`
/// 3. `KITTY_WINDOW_ID` → `KITTY_WINDOW_ID:{val}`
/// 4. `TMUX_PANE` → `tmux:{val}`
/// 5. `STY` → `screen:{val}`
///
/// On Linux, falls back to session ID + TTY + process start time:
/// `sid:{sid}:{tty}:{starttime}`
///
/// Returns `None` if no identifier could be determined.
///
/// This function never panics.
pub fn terminal_session_id() -> Option<String> {
    // Check environment variables in priority order
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

    #[test]
    fn test_with_term_session_id() {
        // Save original value if any
        let original = env::var_os("TERM_SESSION_ID");

        env::set_var("TERM_SESSION_ID", "test123");

        // Clear other session vars to ensure TERM_SESSION_ID wins
        let wt_original = env::var_os("WT_SESSION");
        let kitty_original = env::var_os("KITTY_WINDOW_ID");
        let tmux_original = env::var_os("TMUX_PANE");
        let sty_original = env::var_os("STY");
        env::remove_var("WT_SESSION");
        env::remove_var("KITTY_WINDOW_ID");
        env::remove_var("TMUX_PANE");
        env::remove_var("STY");

        let result = terminal_session_id();
        assert_eq!(result, Some("TERM_SESSION_ID:test123".to_string()));

        // Restore original values
        match original {
            Some(val) => env::set_var("TERM_SESSION_ID", val),
            None => env::remove_var("TERM_SESSION_ID"),
        }
        match wt_original {
            Some(val) => env::set_var("WT_SESSION", val),
            None => (),
        }
        match kitty_original {
            Some(val) => env::set_var("KITTY_WINDOW_ID", val),
            None => (),
        }
        match tmux_original {
            Some(val) => env::set_var("TMUX_PANE", val),
            None => (),
        }
        match sty_original {
            Some(val) => env::set_var("STY", val),
            None => (),
        }
    }

    #[test]
    fn test_no_panic() {
        // This test ensures the function never panics
        // We can't easily test the fallback in a unit test environment,
        // but we can verify it doesn't panic
        let _ = terminal_session_id();
    }
}
