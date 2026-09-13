//! Read-state value type for session unread tracking.
//!
//! The read-state persists a monotonic cursor and manual-unread flag,
//! supporting the `is_unread` predicate: `(last_attention_seq > last_read_seq) || manual_unread`.

use serde::{Deserialize, Serialize};

/// Session read-state persisted to NATS KV.
///
/// Stored at `sessions/{id}/read/default` with `#[serde(default)]` for forward-compatibility.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionReadState {
    /// Highest sequence number that caused attention (e.g., TurnEnd, HitlApprovalRequested).
    pub last_attention_seq: u64,
    /// Highest sequence the session has been read through.
    pub last_read_seq: u64,
    /// Manual unread flag set by user; cleared on mark-read.
    pub manual_unread: bool,
}

impl SessionReadState {
    /// Returns true if the session is considered unread.
    ///
    /// A session is unread if attention exceeds the read cursor OR the manual unread flag is set.
    pub fn is_unread(&self) -> bool {
        (self.last_attention_seq > self.last_read_seq) || self.manual_unread
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_unread_true_when_attention_exceeds_read() {
        let state = SessionReadState {
            last_attention_seq: 10,
            last_read_seq: 5,
            manual_unread: false,
        };
        assert!(state.is_unread());
    }

    #[test]
    fn is_unread_false_when_read_catches_attention() {
        let state = SessionReadState {
            last_attention_seq: 10,
            last_read_seq: 10,
            manual_unread: false,
        };
        assert!(!state.is_unread());
    }

    #[test]
    fn is_unread_true_when_manual_unread_set() {
        let state = SessionReadState {
            last_attention_seq: 10,
            last_read_seq: 15, // read > attention, but manual flag overrides
            manual_unread: true,
        };
        assert!(state.is_unread());
    }

    #[test]
    fn is_unread_false_when_default() {
        let state = SessionReadState::default();
        assert!(!state.is_unread());
    }

    #[test]
    fn is_unread_truth_table() {
        // Test all combinations
        struct Case {
            attention: u64,
            read: u64,
            manual: bool,
            expected: bool,
        }
        let cases = [
            Case {
                attention: 0,
                read: 0,
                manual: false,
                expected: false,
            },
            Case {
                attention: 5,
                read: 0,
                manual: false,
                expected: true,
            },
            Case {
                attention: 5,
                read: 5,
                manual: false,
                expected: false,
            },
            Case {
                attention: 5,
                read: 10,
                manual: false,
                expected: false,
            },
            Case {
                attention: 0,
                read: 0,
                manual: true,
                expected: true,
            },
            Case {
                attention: 5,
                read: 5,
                manual: true,
                expected: true,
            },
            Case {
                attention: 5,
                read: 10,
                manual: true,
                expected: true,
            },
        ];
        for case in cases {
            let state = SessionReadState {
                last_attention_seq: case.attention,
                last_read_seq: case.read,
                manual_unread: case.manual,
            };
            assert_eq!(
                state.is_unread(),
                case.expected,
                "attention={} read={} manual={} => expected={}",
                case.attention,
                case.read,
                case.manual,
                case.expected
            );
        }
    }

    #[test]
    fn serde_round_trip_preserves_fields() {
        let state = SessionReadState {
            last_attention_seq: 42,
            last_read_seq: 17,
            manual_unread: true,
        };
        let json = serde_json::to_string(&state).unwrap();
        let parsed: SessionReadState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, state);
    }

    #[test]
    fn serde_default_allows_missing_fields() {
        // Empty JSON should deserialize to defaults
        let parsed: SessionReadState = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed, SessionReadState::default());

        // Partial JSON should fill missing fields with defaults
        let parsed: SessionReadState =
            serde_json::from_str(r#"{"last_attention_seq": 5}"#).unwrap();
        assert_eq!(parsed.last_attention_seq, 5);
        assert_eq!(parsed.last_read_seq, 0);
        assert!(!parsed.manual_unread);
    }
}
