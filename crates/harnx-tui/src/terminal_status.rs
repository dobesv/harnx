//! Terminal status emission via OSC escape codes.
//!
//! Emits OSC 9999 (Orca) + OSC 9;4 (kitty/JetBrains) sequences to signal
//! agent working/blocked/done/interrupted/error state to compatible terminals.
//!
//! Gated on: `IS_STDOUT_TERMINAL && TERM != "dumb" && CI unset && enabled`.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::LazyLock;
use std::sync::Mutex;

/// Agent status values emitted to the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalStatus {
    /// Agent is actively working (indeterminate progress).
    Working,
    /// Agent is blocked waiting for user input (HITL).
    Blocked,
    /// Agent completed successfully.
    Done,
    /// Agent was interrupted by user (Ctrl+C).
    Interrupted,
    /// Agent encountered an error.
    Error,
    /// Clear the status indicator.
    Clear,
}

impl TerminalStatus {
    /// Returns true if this is a "sticky" failure state.
    /// Error and Interrupted should not be downgraded to Done.
    fn is_sticky_failure(&self) -> bool {
        matches!(self, TerminalStatus::Error | TerminalStatus::Interrupted)
    }

    /// Returns true if this is an "active" (non-clear) state.
    fn is_active(&self) -> bool {
        !matches!(self, TerminalStatus::Clear)
    }

    /// Build the OSC byte sequences for this status.
    /// Returns a single buffer containing both OSC 9;4 and OSC 9999 sequences.
    fn osc_bytes(self) -> &'static [u8] {
        // Pre-computed const byte slices for each variant.
        // OSC 9;4: ESC ] 9 ; 4 ; <state> ST
        // OSC 9999: ESC ] 9999 ; {"state":"<state>"} ST
        match self {
            TerminalStatus::Working => b"\x1b]9;4;3\x1b\\\x1b]9999;{\"state\":\"working\"}\x1b\\",
            TerminalStatus::Blocked => b"\x1b]9;4;4\x1b\\\x1b]9999;{\"state\":\"blocked\"}\x1b\\",
            TerminalStatus::Done => b"\x1b]9;4;0\x1b\\\x1b]9999;{\"state\":\"done\"}\x1b\\",
            TerminalStatus::Interrupted => {
                b"\x1b]9;4;0\x1b\\\x1b]9999;{\"state\":\"interrupted\"}\x1b\\"
            }
            TerminalStatus::Error => {
                // orcatui rejects "failed", use "interrupted" for compatibility
                b"\x1b]9;4;2\x1b\\\x1b]9999;{\"state\":\"interrupted\"}\x1b\\"
            }
            TerminalStatus::Clear => b"\x1b]9;4;0\x1b\\\x1b]9999;{\"state\":\"done\"}\x1b\\",
        }
    }
}

/// Process-global terminal status state.
/// Test code should construct their own `TerminalStatusState::new(true)` with a `Vec<u8>` writer.
pub struct TerminalStatusState {
    /// Computed once at init: IS_STDOUT_TERMINAL && TERM != "dumb" && CI unset
    static_ok: bool,
    /// Live config flag, set at init and by `.set`.
    enabled: AtomicBool,
    /// Last emitted status, for diffing and sticky-failure logic.
    last: Mutex<Option<TerminalStatus>>,
}

impl TerminalStatusState {
    /// Create a new state with the given `static_ok` value.
    /// Tests should pass `true` to bypass TTY/TERM/CI checks.
    pub fn new(static_ok: bool) -> Self {
        Self {
            static_ok,
            enabled: AtomicBool::new(false),
            last: Mutex::new(None),
        }
    }

    /// Check if emission is currently enabled.
    /// Returns `static_ok && enabled`.
    pub fn is_on(&self) -> bool {
        self.static_ok && self.enabled.load(Ordering::SeqCst)
    }

    /// Set the terminal status.
    ///
    /// Respects gating, diffing, and sticky-failure rules:
    /// 1. If `!is_on()` -> no-op.
    /// 2. If poisoned lock, handle safely.
    /// 3. If `next == last` -> no-op (diffing).
    /// 4. Sticky failure: if `last ∈ {Error, Interrupted}` and `next == Done` -> no-op.
    /// 5. `Working` always resets sticky and emits.
    /// 6. Otherwise write bytes, flush, and update `last`.
    pub fn set_status(&self, next: TerminalStatus, writer: &mut impl Write) {
        // 1. Gating check
        if !self.is_on() {
            return;
        }

        // 2. Handle poisoned lock safely
        let mut last_guard = match self.last.lock() {
            Ok(guard) => guard,
            Err(e) => e.into_inner(),
        };

        let last = *last_guard;

        // 3. Diffing: same state -> no-op
        if last == Some(next) {
            return;
        }

        // 4. Sticky failure: don't downgrade Error/Interrupted to Done
        if let Some(prev) = last {
            if prev.is_sticky_failure() && next == TerminalStatus::Done {
                return;
            }
        }

        // 5. Working always resets sticky and emits
        // (handled implicitly by the update below)

        // 6. Emit and update
        let bytes = next.osc_bytes();
        let _ = writer.write_all(bytes);
        let _ = writer.flush();

        *last_guard = Some(next);
    }

    /// Force clear the status indicator.
    /// Still respects `is_on()` gating. Bypasses diffing.
    /// Preserves `self.last` so that `restore()` can re-emit the active state
    /// after external-editor suspend/resume.
    pub fn force_clear(&self, writer: &mut impl Write) {
        if !self.is_on() {
            return;
        }

        let bytes = TerminalStatus::Clear.osc_bytes();
        let _ = writer.write_all(bytes);
        let _ = writer.flush();

        // Do NOT update self.last — preserve it for restore() to re-emit.
    }

    /// Restore the last active status.
    /// Re-emits `last` if `is_on()` and `last` is an active state.
    /// Used after external terminal clear (e.g., external-editor suspend/resume).
    pub fn restore(&self, writer: &mut impl Write) {
        if !self.is_on() {
            return;
        }

        let last = match self.last.lock() {
            Ok(guard) => *guard,
            Err(e) => *e.into_inner(),
        };

        // Only restore if there's an active (non-clear) state
        if let Some(status) = last {
            if status.is_active() {
                let bytes = status.osc_bytes();
                let _ = writer.write_all(bytes);
                let _ = writer.flush();
            }
        }
    }

    /// Enable or disable status emission.
    /// When flipping `true -> false` and `last` is an active non-clear state,
    /// emits one forced Clear before going silent.
    pub fn set_enabled(&self, on: bool, writer: &mut impl Write) {
        let was_on = self.enabled.swap(on, Ordering::SeqCst);

        // If disabling and last was active, emit clear
        if was_on && !on {
            // Check last state
            let last = match self.last.lock() {
                Ok(guard) => *guard,
                Err(e) => *e.into_inner(),
            };

            if let Some(status) = last {
                if status.is_active() {
                    let bytes = TerminalStatus::Clear.osc_bytes();
                    let _ = writer.write_all(bytes);
                    let _ = writer.flush();
                }
            }
        }
    }
}

/// Compute whether the static environment permits emission.
/// Checks: IS_STDOUT_TERMINAL && TERM != "dumb" && CI unset
fn compute_static_ok() -> bool {
    use harnx_runtime::utils::IS_STDOUT_TERMINAL;

    evaluate_static_ok(
        *IS_STDOUT_TERMINAL,
        std::env::var("TERM").ok().as_deref(),
        std::env::var("CI").is_ok(),
    )
}

/// Pure function for unit testing the environment gating logic.
/// Returns true if:
/// - `is_terminal` is true (stdout is a TTY)
/// - `term` is not "dumb" (either Some("xterm-256color") or None)
/// - `ci_set` is false (CI environment variable is not set)
pub(crate) fn evaluate_static_ok(is_terminal: bool, term: Option<&str>, ci_set: bool) -> bool {
    if !is_terminal {
        return false;
    }

    // Check TERM != "dumb"
    if term == Some("dumb") {
        return false;
    }

    // Check CI not set
    if ci_set {
        return false;
    }

    true
}

/// Process-global terminal status state.
pub static TERMINAL_STATUS: LazyLock<TerminalStatusState> =
    LazyLock::new(|| TerminalStatusState::new(compute_static_ok()));

/// Set the terminal status (delegates to global, writes to stdout).
pub fn set_status(next: TerminalStatus) {
    let mut stdout = std::io::stdout();
    TERMINAL_STATUS.set_status(next, &mut stdout);
}

/// Force clear the status (delegates to global, writes to stdout).
pub fn force_clear() {
    let mut stdout = std::io::stdout();
    TERMINAL_STATUS.force_clear(&mut stdout);
}

/// Restore the last active status (delegates to global, writes to stdout).
pub fn restore() {
    let mut stdout = std::io::stdout();
    TERMINAL_STATUS.restore(&mut stdout);
}

/// Enable or disable status emission (delegates to global, writes to stdout).
pub fn set_enabled(on: bool) {
    let mut stdout = std::io::stdout();
    TERMINAL_STATUS.set_enabled(on, &mut stdout);
}

// =========================================================================
// Unit tests for evaluate_static_ok() environment gating
// =========================================================================

#[cfg(test)]
mod static_ok_tests {
    use super::*;

    #[test]
    fn test_not_terminal_returns_false() {
        // When is_terminal is false, always false
        assert!(!evaluate_static_ok(false, Some("xterm-256color"), false));
        assert!(!evaluate_static_ok(false, None, false));
        assert!(!evaluate_static_ok(false, Some("dumb"), false));
    }

    #[test]
    fn test_dumb_term_returns_false() {
        // TERM="dumb" should block emission
        assert!(!evaluate_static_ok(true, Some("dumb"), false));
    }

    #[test]
    fn test_ci_set_returns_false() {
        // CI environment variable set should block emission
        assert!(!evaluate_static_ok(true, Some("xterm-256color"), true));
        assert!(!evaluate_static_ok(true, None, true));
    }

    #[test]
    fn test_all_conditions_met_returns_true() {
        // is_terminal=true, term is a proper value, CI not set => true
        assert!(evaluate_static_ok(true, Some("xterm-256color"), false));
        assert!(evaluate_static_ok(true, Some("screen"), false));
        assert!(evaluate_static_ok(true, Some("xterm"), false));
    }

    #[test]
    fn test_term_none_returns_true() {
        // TERM unset (None) is acceptable - treats as non-dumb
        assert!(evaluate_static_ok(true, None, false));
    }

    #[test]
    fn test_all_false_except_terminal_returns_true() {
        // is_terminal=true with term=None and CI not set
        assert!(evaluate_static_ok(true, None, false));
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn collect_output(state: &TerminalStatusState, status: TerminalStatus) -> Vec<u8> {
        let mut buf = Vec::new();
        state.set_status(status, &mut buf);
        buf
    }

    #[test]
    fn test_working_emits_correct_bytes() {
        let state = TerminalStatusState::new(true);
        state.set_enabled(true, &mut Vec::new());

        let output = collect_output(&state, TerminalStatus::Working);

        // OSC 9;4;3 ST + OSC 9999;{"state":"working"} ST
        assert!(output.starts_with(b"\x1b]9;4;3\x1b\\"));
        assert!(output.ends_with(b"\x1b\\"));
        assert!(std::str::from_utf8(&output)
            .unwrap()
            .contains(r#"{"state":"working"}"#));
    }

    #[test]
    fn test_blocked_emits_correct_bytes() {
        let state = TerminalStatusState::new(true);
        state.set_enabled(true, &mut Vec::new());

        let output = collect_output(&state, TerminalStatus::Blocked);

        assert!(output.starts_with(b"\x1b]9;4;4\x1b\\"));
        assert!(std::str::from_utf8(&output)
            .unwrap()
            .contains(r#"{"state":"blocked"}"#));
    }

    #[test]
    fn test_done_emits_correct_bytes() {
        let state = TerminalStatusState::new(true);
        state.set_enabled(true, &mut Vec::new());

        let output = collect_output(&state, TerminalStatus::Done);

        assert!(output.starts_with(b"\x1b]9;4;0\x1b\\"));
        assert!(std::str::from_utf8(&output)
            .unwrap()
            .contains(r#"{"state":"done"}"#));
    }

    #[test]
    fn test_interrupted_emits_correct_bytes() {
        let state = TerminalStatusState::new(true);
        state.set_enabled(true, &mut Vec::new());

        let output = collect_output(&state, TerminalStatus::Interrupted);

        assert!(output.starts_with(b"\x1b]9;4;0\x1b\\"));
        assert!(std::str::from_utf8(&output)
            .unwrap()
            .contains(r#"{"state":"interrupted"}"#));
    }

    #[test]
    fn test_error_emits_interrupted_json() {
        // Error uses state=2 for OSC 9;4 (red bar) but "interrupted" JSON
        let state = TerminalStatusState::new(true);
        state.set_enabled(true, &mut Vec::new());

        let output = collect_output(&state, TerminalStatus::Error);

        assert!(output.starts_with(b"\x1b]9;4;2\x1b\\"));
        assert!(std::str::from_utf8(&output)
            .unwrap()
            .contains(r#"{"state":"interrupted"}"#));
    }

    #[test]
    fn test_clear_emits_done() {
        let state = TerminalStatusState::new(true);
        state.set_enabled(true, &mut Vec::new());

        let output = collect_output(&state, TerminalStatus::Clear);

        assert!(output.starts_with(b"\x1b]9;4;0\x1b\\"));
        assert!(std::str::from_utf8(&output)
            .unwrap()
            .contains(r#"{"state":"done"}"#));
    }

    #[test]
    fn test_diffing_prevents_redundant_emit() {
        let state = TerminalStatusState::new(true);
        state.set_enabled(true, &mut Vec::new());

        let mut buf = Vec::new();
        state.set_status(TerminalStatus::Working, &mut buf);
        let first_len = buf.len();
        assert!(first_len > 0);

        // Second call with same status should be no-op
        state.set_status(TerminalStatus::Working, &mut buf);
        assert_eq!(buf.len(), first_len, "should not emit duplicate status");
    }

    #[test]
    fn test_sticky_failure_prevents_downgrade_to_done() {
        let state = TerminalStatusState::new(true);
        state.set_enabled(true, &mut Vec::new());

        let mut buf = Vec::new();

        // Emit error
        state.set_status(TerminalStatus::Error, &mut buf);
        let after_error = buf.len();

        // Try to emit Done - should be blocked by sticky rule
        state.set_status(TerminalStatus::Done, &mut buf);
        assert_eq!(buf.len(), after_error, "should not downgrade Error to Done");

        // Working resets sticky
        state.set_status(TerminalStatus::Working, &mut buf);
        assert!(buf.len() > after_error);
    }

    #[test]
    fn test_interrupted_is_sticky() {
        let state = TerminalStatusState::new(true);
        state.set_enabled(true, &mut Vec::new());

        let mut buf = Vec::new();

        state.set_status(TerminalStatus::Interrupted, &mut buf);
        let after_interrupt = buf.len();

        // Done after Interrupted should be blocked
        state.set_status(TerminalStatus::Done, &mut buf);
        assert_eq!(
            buf.len(),
            after_interrupt,
            "should not downgrade Interrupted to Done"
        );
    }

    #[test]
    fn test_disabled_state_emits_nothing() {
        let state = TerminalStatusState::new(true);
        // enabled is false by default

        let mut buf = Vec::new();
        state.set_status(TerminalStatus::Working, &mut buf);
        assert!(buf.is_empty(), "should emit nothing when disabled");
    }

    #[test]
    fn test_static_ok_false_prevents_emission() {
        let state = TerminalStatusState::new(false);
        state.set_enabled(true, &mut Vec::new());

        let mut buf = Vec::new();
        state.set_status(TerminalStatus::Working, &mut buf);
        assert!(
            buf.is_empty(),
            "should emit nothing when static_ok is false"
        );
    }

    #[test]
    fn test_set_enabled_clears_on_disable() {
        let state = TerminalStatusState::new(true);

        let mut buf = Vec::new();
        state.set_enabled(true, &mut buf);

        // Set to an active state
        state.set_status(TerminalStatus::Working, &mut buf);

        // Disable - should emit clear
        let len_before_disable = buf.len();
        state.set_enabled(false, &mut buf);

        // Should have emitted a clear sequence
        assert!(
            buf.len() > len_before_disable,
            "should emit clear when disabling with active state"
        );

        // Further emissions should be silent
        let len_after_disable = buf.len();
        state.set_status(TerminalStatus::Working, &mut buf);
        assert_eq!(
            buf.len(),
            len_after_disable,
            "should not emit after disable"
        );
    }

    #[test]
    fn test_force_clear_bypasses_diffing() {
        let state = TerminalStatusState::new(true);
        state.set_enabled(true, &mut Vec::new());

        let mut buf = Vec::new();

        // Set Clear via set_status
        state.set_status(TerminalStatus::Clear, &mut buf);
        let first_clear_len = buf.len();

        // force_clear should still emit even though last is Clear
        state.force_clear(&mut buf);
        assert!(
            buf.len() > first_clear_len,
            "force_clear should emit even with diffing"
        );
    }

    #[test]
    fn test_restore_reemits_active_state() {
        let state = TerminalStatusState::new(true);
        state.set_enabled(true, &mut Vec::new());

        let mut buf = Vec::new();

        // Set Working
        state.set_status(TerminalStatus::Working, &mut buf);

        // force_clear emits Clear but preserves last for restore()
        state.force_clear(&mut buf);
        let after_clear_len = buf.len();

        // Restore should re-emit Working (the preserved last state)
        state.restore(&mut buf);
        assert!(
            buf.len() > after_clear_len,
            "restore should re-emit active state after force_clear"
        );
        assert_eq!(
            &buf[after_clear_len..],
            TerminalStatus::Working.osc_bytes(),
            "restore should re-emit Working bytes"
        );
    }

    #[test]
    fn test_restore_does_nothing_when_clear() {
        let state = TerminalStatusState::new(true);
        state.set_enabled(true, &mut Vec::new());

        let mut buf = Vec::new();

        // Set Clear
        state.set_status(TerminalStatus::Clear, &mut buf);
        let clear_len = buf.len();

        // Restore should not emit
        state.restore(&mut buf);
        assert_eq!(
            buf.len(),
            clear_len,
            "restore should not emit when last is Clear"
        );
    }

    #[test]
    fn test_restore_respects_gating() {
        let state = TerminalStatusState::new(true);
        state.set_enabled(true, &mut Vec::new());

        let mut buf = Vec::new();
        state.set_status(TerminalStatus::Working, &mut buf);

        // Disable
        state.set_enabled(false, &mut buf);

        // Restore should do nothing when disabled
        let len_before_restore = buf.len();
        state.restore(&mut buf);
        assert_eq!(buf.len(), len_before_restore);
    }

    #[test]
    fn test_is_on() {
        let state = TerminalStatusState::new(true);
        assert!(!state.is_on(), "should be off initially");

        state.set_enabled(true, &mut Vec::new());
        assert!(state.is_on(), "should be on after enable");

        state.set_enabled(false, &mut Vec::new());
        assert!(!state.is_on(), "should be off after disable");
    }

    #[test]
    fn test_transition_sequence_working_to_done() {
        let state = TerminalStatusState::new(true);
        state.set_enabled(true, &mut Vec::new());

        let mut buf = Vec::new();

        state.set_status(TerminalStatus::Working, &mut buf);
        state.set_status(TerminalStatus::Done, &mut buf);

        // Should contain both emissions
        let s = std::str::from_utf8(&buf).unwrap();
        assert!(s.contains(r#"{"state":"working"}"#));
        assert!(s.contains(r#"{"state":"done"}"#));
    }

    // =========================================================================
    // Byte-exact tests for each status variant
    // =========================================================================

    #[test]
    fn test_working_byte_exact() {
        let state = TerminalStatusState::new(true);
        state.set_enabled(true, &mut Vec::new());

        let mut buf = Vec::new();
        state.set_status(TerminalStatus::Working, &mut buf);

        // Exact expected bytes: OSC 9;4;3 ST + OSC 9999;{"state":"working"} ST
        let expected = b"\x1b]9;4;3\x1b\\\x1b]9999;{\"state\":\"working\"}\x1b\\";
        assert_eq!(
            &buf[..],
            expected,
            "Working should emit exact OSC sequences"
        );
    }

    #[test]
    fn test_blocked_byte_exact() {
        let state = TerminalStatusState::new(true);
        state.set_enabled(true, &mut Vec::new());

        let mut buf = Vec::new();
        state.set_status(TerminalStatus::Blocked, &mut buf);

        // Exact expected bytes: OSC 9;4;4 ST + OSC 9999;{"state":"blocked"} ST
        let expected = b"\x1b]9;4;4\x1b\\\x1b]9999;{\"state\":\"blocked\"}\x1b\\";
        assert_eq!(
            &buf[..],
            expected,
            "Blocked should emit exact OSC sequences"
        );
    }

    #[test]
    fn test_done_byte_exact() {
        let state = TerminalStatusState::new(true);
        state.set_enabled(true, &mut Vec::new());

        let mut buf = Vec::new();
        state.set_status(TerminalStatus::Done, &mut buf);

        // Exact expected bytes: OSC 9;4;0 ST + OSC 9999;{"state":"done"} ST
        let expected = b"\x1b]9;4;0\x1b\\\x1b]9999;{\"state\":\"done\"}\x1b\\";
        assert_eq!(&buf[..], expected, "Done should emit exact OSC sequences");
    }

    #[test]
    fn test_interrupted_byte_exact() {
        let state = TerminalStatusState::new(true);
        state.set_enabled(true, &mut Vec::new());

        let mut buf = Vec::new();
        state.set_status(TerminalStatus::Interrupted, &mut buf);

        // Exact expected bytes: OSC 9;4;0 ST + OSC 9999;{"state":"interrupted"} ST
        let expected = b"\x1b]9;4;0\x1b\\\x1b]9999;{\"state\":\"interrupted\"}\x1b\\";
        assert_eq!(
            &buf[..],
            expected,
            "Interrupted should emit exact OSC sequences"
        );
    }

    #[test]
    fn test_error_byte_exact() {
        let state = TerminalStatusState::new(true);
        state.set_enabled(true, &mut Vec::new());

        let mut buf = Vec::new();
        state.set_status(TerminalStatus::Error, &mut buf);

        // Exact expected bytes: OSC 9;4;2 ST + OSC 9999;{"state":"interrupted"} ST
        // Note: Error uses state=2 (red bar) but "interrupted" JSON for orcatui compatibility
        let expected = b"\x1b]9;4;2\x1b\\\x1b]9999;{\"state\":\"interrupted\"}\x1b\\";
        assert_eq!(
            &buf[..],
            expected,
            "Error should emit exact OSC sequences with 'interrupted' JSON"
        );
    }

    #[test]
    fn test_clear_byte_exact() {
        let state = TerminalStatusState::new(true);
        state.set_enabled(true, &mut Vec::new());

        let mut buf = Vec::new();
        state.set_status(TerminalStatus::Clear, &mut buf);

        // Exact expected bytes: OSC 9;4;0 ST + OSC 9999;{"state":"done"} ST
        let expected = b"\x1b]9;4;0\x1b\\\x1b]9999;{\"state\":\"done\"}\x1b\\";
        assert_eq!(&buf[..], expected, "Clear should emit exact OSC sequences");
    }

    // =========================================================================
    // Diffing tests: emitting same status consecutively is a no-op
    // =========================================================================

    #[test]
    fn test_diffing_same_status_twice_emits_once() {
        let state = TerminalStatusState::new(true);
        state.set_enabled(true, &mut Vec::new());

        let mut buf = Vec::new();
        state.set_status(TerminalStatus::Working, &mut buf);
        let first_len = buf.len();

        // Second call with same status should be a no-op (zero additional bytes)
        state.set_status(TerminalStatus::Working, &mut buf);
        assert_eq!(
            buf.len(),
            first_len,
            "same status twice should emit only once"
        );
    }

    // =========================================================================
    // Sticky failure tests: Error -> Done and Interrupted -> Done should not emit Done
    // =========================================================================

    #[test]
    fn test_sticky_error_blocks_done() {
        let state = TerminalStatusState::new(true);
        state.set_enabled(true, &mut Vec::new());

        let mut buf = Vec::new();

        // Working -> Error sequence
        state.set_status(TerminalStatus::Working, &mut buf);
        state.set_status(TerminalStatus::Error, &mut buf);
        let after_error = buf.len();

        // Done after Error should NOT emit (sticky failure rule)
        state.set_status(TerminalStatus::Done, &mut buf);
        assert_eq!(
            buf.len(),
            after_error,
            "Done after Error should be blocked (sticky)"
        );

        // Verify we have Working + Error only, no Done bytes
        let s = std::str::from_utf8(&buf).unwrap();
        assert!(s.contains(r#"{"state":"working"}"#), "should have Working");
        assert!(
            s.contains(r#"{"state":"interrupted"}"#),
            "should have Error (interrupted JSON)"
        );
        assert!(!s.contains(r#"{"state":"done"}"#), "should NOT have Done");
    }

    #[test]
    fn test_sticky_interrupted_blocks_done() {
        let state = TerminalStatusState::new(true);
        state.set_enabled(true, &mut Vec::new());

        let mut buf = Vec::new();

        // Working -> Interrupted sequence
        state.set_status(TerminalStatus::Working, &mut buf);
        state.set_status(TerminalStatus::Interrupted, &mut buf);
        let after_interrupted = buf.len();

        // Done after Interrupted should NOT emit (sticky failure rule)
        state.set_status(TerminalStatus::Done, &mut buf);
        assert_eq!(
            buf.len(),
            after_interrupted,
            "Done after Interrupted should be blocked (sticky)"
        );

        // Verify we have Working + Interrupted only, no Done bytes
        let s = std::str::from_utf8(&buf).unwrap();
        assert!(s.contains(r#"{"state":"working"}"#), "should have Working");
        assert!(
            s.contains(r#"{"state":"interrupted"}"#),
            "should have Interrupted"
        );
        assert!(!s.contains(r#"{"state":"done"}"#), "should NOT have Done");
    }

    #[test]
    fn test_normal_working_to_done_emits_both() {
        let state = TerminalStatusState::new(true);
        state.set_enabled(true, &mut Vec::new());

        let mut buf = Vec::new();

        // Normal case: Working -> Done should emit both
        state.set_status(TerminalStatus::Working, &mut buf);
        state.set_status(TerminalStatus::Done, &mut buf);

        // Should contain both emissions
        let s = std::str::from_utf8(&buf).unwrap();
        assert!(s.contains(r#"{"state":"working"}"#), "should have Working");
        assert!(s.contains(r#"{"state":"done"}"#), "should have Done");
    }

    #[test]
    fn test_working_resets_sticky_failure() {
        let state = TerminalStatusState::new(true);
        state.set_enabled(true, &mut Vec::new());

        let mut buf = Vec::new();

        // Error then Working should emit Working (resets sticky)
        state.set_status(TerminalStatus::Error, &mut buf);
        let after_error = buf.len();
        state.set_status(TerminalStatus::Working, &mut buf);

        assert!(
            buf.len() > after_error,
            "Working should emit after Error (resets sticky)"
        );

        // And then Done should work normally after the reset
        state.set_status(TerminalStatus::Done, &mut buf);
        let s = std::str::from_utf8(&buf).unwrap();
        assert!(
            s.contains(r#"{"state":"working"}"#),
            "should have Working after reset"
        );
        assert!(
            s.contains(r#"{"state":"done"}"#),
            "should have Done after sticky reset"
        );
    }

    // =========================================================================
    // Gating tests: static_ok=false and enabled=false produce zero bytes
    // =========================================================================

    #[test]
    fn test_static_ok_false_produces_zero_bytes() {
        // static_ok = false simulates non-TTY, TERM=dumb, or CI environment
        let state = TerminalStatusState::new(false);
        state.set_enabled(true, &mut Vec::new());

        let mut buf = Vec::new();
        state.set_status(TerminalStatus::Working, &mut buf);
        assert!(buf.is_empty(), "static_ok=false should produce zero bytes");
    }

    #[test]
    fn test_enabled_false_produces_zero_bytes() {
        let state = TerminalStatusState::new(true);
        // enabled is false by default

        let mut buf = Vec::new();
        state.set_status(TerminalStatus::Working, &mut buf);
        assert!(buf.is_empty(), "enabled=false should produce zero bytes");
    }

    #[test]
    fn test_both_gates_must_be_true() {
        // Both static_ok AND enabled must be true for emission
        let state = TerminalStatusState::new(false);
        state.set_enabled(true, &mut Vec::new()); // enabled=true, but static_ok=false

        let mut buf = Vec::new();
        state.set_status(TerminalStatus::Working, &mut buf);
        assert!(buf.is_empty(), "both static_ok and enabled must be true");
    }

    // =========================================================================
    // Runtime toggle: set_enabled(false) emits Clear, then silence
    // =========================================================================

    #[test]
    fn test_runtime_disable_emits_clear_then_silence() {
        let state = TerminalStatusState::new(true);

        let mut buf = Vec::new();

        // Enable and emit Working
        state.set_enabled(true, &mut buf);
        state.set_status(TerminalStatus::Working, &mut buf);
        let len_after_working = buf.len();

        // Disable: should emit Clear (single Clear sequence)
        state.set_enabled(false, &mut buf);
        assert!(
            buf.len() > len_after_working,
            "set_enabled(false) should emit Clear"
        );

        // Subsequent set_status should produce zero bytes
        let len_after_disable = buf.len();
        state.set_status(TerminalStatus::Working, &mut buf);
        assert_eq!(
            buf.len(),
            len_after_disable,
            "after disable, set_status should produce zero bytes"
        );

        // And set_status Done should also produce zero bytes
        state.set_status(TerminalStatus::Done, &mut buf);
        assert_eq!(
            buf.len(),
            len_after_disable,
            "after disable, Done should also produce zero bytes"
        );
    }

    // =========================================================================
    // restore() tests: re-emit after force_clear, no-op when off/clear
    // =========================================================================

    #[test]
    fn test_restore_reemits_working_after_force_clear() {
        let state = TerminalStatusState::new(true);
        state.set_enabled(true, &mut Vec::new());

        let mut buf = Vec::new();

        // Set Working
        state.set_status(TerminalStatus::Working, &mut buf);

        // force_clear emits Clear but preserves "last" for restore()
        state.force_clear(&mut buf);
        let after_clear_len = buf.len();

        // Restore should re-emit Working (the preserved active state)
        state.restore(&mut buf);
        assert!(
            buf.len() > after_clear_len,
            "restore() should re-emit Working after force_clear"
        );

        // Verify the restored bytes exactly match Working
        let expected_working = b"\x1b]9;4;3\x1b\\\x1b]9999;{\"state\":\"working\"}\x1b\\";
        assert_eq!(
            &buf[after_clear_len..],
            expected_working,
            "restore should re-emit exact Working bytes"
        );
    }

    #[test]
    fn test_restore_no_op_when_disabled() {
        let state = TerminalStatusState::new(true);
        state.set_enabled(true, &mut Vec::new());

        let mut buf = Vec::new();
        state.set_status(TerminalStatus::Working, &mut buf);

        // Disable (emits Clear)
        state.set_enabled(false, &mut buf);
        let len_before_restore = buf.len();

        // restore() should be a no-op when disabled
        state.restore(&mut buf);
        assert_eq!(
            buf.len(),
            len_before_restore,
            "restore() should be no-op when disabled"
        );
    }

    #[test]
    fn test_restore_no_op_when_last_is_clear() {
        let state = TerminalStatusState::new(true);
        state.set_enabled(true, &mut Vec::new());

        let mut buf = Vec::new();

        // Set Clear (which is NOT active per is_active())
        state.set_status(TerminalStatus::Clear, &mut buf);
        let len_after_clear = buf.len();

        // restore() should be no-op when last is Clear (Clear is not active)
        state.restore(&mut buf);
        assert_eq!(
            buf.len(),
            len_after_clear,
            "restore() should be no-op when last is Clear"
        );
    }

    #[test]
    fn test_restore_reemits_done_since_done_is_active() {
        let state = TerminalStatusState::new(true);
        state.set_enabled(true, &mut Vec::new());

        let mut buf = Vec::new();

        // Set Done - this IS an active state per is_active()
        state.set_status(TerminalStatus::Done, &mut buf);
        let len_after_done = buf.len();

        // restore() SHOULD re-emit Done since Done is active
        state.restore(&mut buf);
        assert!(
            buf.len() > len_after_done,
            "restore() should re-emit Done (Done is active)"
        );
    }

    // =========================================================================
    // Error uses "interrupted" (never "failed") in OSC 9999 JSON
    // =========================================================================

    #[test]
    fn test_error_never_uses_failed_in_json() {
        let state = TerminalStatusState::new(true);
        state.set_enabled(true, &mut Vec::new());

        let mut buf = Vec::new();
        state.set_status(TerminalStatus::Error, &mut buf);

        let s = std::str::from_utf8(&buf).unwrap();

        // Must use "interrupted" for orcatui compatibility
        assert!(
            s.contains(r#"{"state":"interrupted"}"#),
            "Error must use 'interrupted' JSON"
        );
        assert!(!s.contains("failed"), "Error must not use 'failed' in JSON");

        // Must use OSC 9;4;2 (red bar) for the progress sequence
        assert!(
            s.starts_with("\x1b]9;4;2\x1b\\"),
            "Error must use OSC 9;4;2 (red bar)"
        );
    }
}
