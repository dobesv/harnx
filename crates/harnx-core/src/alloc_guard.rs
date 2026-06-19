//! A global-allocator wrapper that aborts with a backtrace the instant live
//! heap usage crosses a configured ceiling.
//!
//! This exists to catch the intermittent runaway-allocation OOM (#842): the
//! process ballooned to ~41 GB faster than the once-per-second TUI watchdog
//! could sample it, and the kernel OOM-kill leaves no stack. With this guard
//! armed (set `HARNX_HEAP_LIMIT_MB`), the *first* allocation that pushes live
//! heap past the limit captures a backtrace — whose top frames are the runaway
//! allocation site — writes it to stderr and the log file, then aborts before
//! the machine is exhausted.
//!
//! Armed by default at [`DEFAULT_LIMIT_MB`] MiB so the next runaway is caught
//! automatically; `HARNX_HEAP_LIMIT_MB` overrides the ceiling, and `0` disables
//! the guard entirely.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Default live-heap ceiling (MiB) when `HARNX_HEAP_LIMIT_MB` is unset. Sits far
/// above any healthy session yet far below the ~41 GiB #842 blow-up, so a
/// runaway trips here — with a backtrace — long before the machine is exhausted.
pub const DEFAULT_LIMIT_MB: usize = 4096;

/// Live (allocated − freed) byte ceiling; `0` means disarmed. Initialised to the
/// default so the guard is armed from the very first allocation. Counting from
/// process start (rather than arming mid-run) is what keeps [`LIVE_BYTES`]
/// balanced: every `dealloc` matches an `alloc` that was counted, so the live
/// total can't underflow.
static LIMIT_BYTES: AtomicUsize = AtomicUsize::new(DEFAULT_LIMIT_MB * 1024 * 1024);
/// Running live heap bytes (every `alloc` adds, every `dealloc` subtracts).
static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
/// Set once when the limit is first breached so the backtrace-capture path (it
/// allocates) cannot recurse back into the breach handler.
static TRIPPED: AtomicBool = AtomicBool::new(false);

/// Apply a `HARNX_HEAP_LIMIT_MB` override on top of the default ceiling. Unset
/// keeps the default; `0` disarms; any other value sets that many MiB; garbage
/// is ignored (default kept). Call once, early in `main`.
pub fn init_from_env() {
    let raw = match std::env::var("HARNX_HEAP_LIMIT_MB") {
        Ok(v) => v,
        Err(_) => {
            log::debug!(
                "heap guard armed at default {DEFAULT_LIMIT_MB} MiB (set HARNX_HEAP_LIMIT_MB to override, 0 to disable)"
            );
            return;
        }
    };
    match parse_override(&raw) {
        Some(0) => {
            LIMIT_BYTES.store(0, Ordering::Relaxed);
            log::warn!("heap guard disabled (HARNX_HEAP_LIMIT_MB=0)");
        }
        Some(mb) => {
            LIMIT_BYTES.store(mb.saturating_mul(1024 * 1024), Ordering::Relaxed);
            log::warn!("heap guard armed at {mb} MiB (HARNX_HEAP_LIMIT_MB); aborts with a backtrace if exceeded");
        }
        None => log::warn!(
            "ignoring invalid HARNX_HEAP_LIMIT_MB={raw:?}; keeping default {DEFAULT_LIMIT_MB} MiB"
        ),
    }
}

/// Parse a `HARNX_HEAP_LIMIT_MB` override: `Some(mb)` (including `Some(0)` to
/// disable) for a valid non-negative integer, `None` for empty/garbage.
fn parse_override(raw: &str) -> Option<usize> {
    raw.trim().parse::<usize>().ok()
}

/// Global allocator that enforces [`init_from_env`]'s ceiling. Wraps [`System`].
pub struct HeapGuard;

unsafe impl GlobalAlloc for HeapGuard {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let limit = LIMIT_BYTES.load(Ordering::Relaxed);
        if limit != 0 {
            let live = LIVE_BYTES
                .fetch_add(layout.size(), Ordering::Relaxed)
                .saturating_add(layout.size());
            if live > limit && !TRIPPED.swap(true, Ordering::SeqCst) {
                // We are the first allocation over the line. TRIPPED is now set,
                // so the allocations made while capturing the backtrace below
                // take the `!swap` = false branch and never re-enter here.
                report_and_abort(live, limit);
            }
        }
        System.alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if LIMIT_BYTES.load(Ordering::Relaxed) != 0 {
            LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
        }
        System.dealloc(ptr, layout)
    }
}

#[cold]
#[inline(never)]
fn report_and_abort(live: usize, limit: usize) -> ! {
    let bt = std::backtrace::Backtrace::force_capture();
    let report = format!("{}\n{bt}\n", trip_header(live, limit));
    eprint!("{report}");
    // Also append to the log file if one is configured — it survives the abort
    // and is where the user is already collecting diagnostics.
    if let Ok(path) = std::env::var("HARNX_LOG_PATH") {
        if !path.is_empty() {
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                use std::io::Write;
                let _ = f.write_all(report.as_bytes());
                let _ = f.flush();
            }
        }
    }
    std::process::abort();
}

/// Human-readable header describing a breach (the backtrace is appended after).
/// Split out so the wording is unit-testable without aborting the process.
fn trip_header(live: usize, limit: usize) -> String {
    let mib = |b: usize| b / (1024 * 1024);
    format!(
        "=== harnx heap guard tripped (pid {}) ===\n\
         live heap ~{} MiB exceeded the limit of {} MiB \
         (HARNX_HEAP_LIMIT_MB; default {DEFAULT_LIMIT_MB}).\n\
         Backtrace of the allocation that crossed the limit (top frames = culprit):",
        std::process::id(),
        mib(live),
        mib(limit),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::alloc::{GlobalAlloc, Layout};

    #[test]
    fn parses_override_including_zero_to_disable() {
        assert_eq!(parse_override("8"), Some(8));
        assert_eq!(parse_override("  6000 "), Some(6000));
        assert_eq!(parse_override("0"), Some(0));
    }

    #[test]
    fn rejects_empty_and_garbage_override() {
        assert_eq!(parse_override(""), None);
        assert_eq!(parse_override("nope"), None);
        assert_eq!(parse_override("-5"), None);
    }

    #[test]
    fn small_allocation_under_default_limit_passes_through() {
        // The guard is armed at the 4 GiB default in this test process; a tiny
        // allocation is well under it, so alloc/free must work without tripping.
        let layout = Layout::from_size_align(1024, 8).unwrap();
        unsafe {
            let p = HeapGuard.alloc(layout);
            assert!(!p.is_null());
            *p = 7;
            assert_eq!(*p, 7);
            HeapGuard.dealloc(p, layout);
        }
    }

    #[test]
    fn trip_header_reports_sizes_and_env_var() {
        let header = trip_header(5_000 * 1024 * 1024, 4096 * 1024 * 1024);
        assert!(header.contains("~5000 MiB"));
        assert!(header.contains("limit of 4096 MiB"));
        assert!(header.contains("HARNX_HEAP_LIMIT_MB"));
    }
}
