//! Core NATS request/reply protocol for tool reservations.
//!
//! Reserve uses a shared queue or a frontend-owned local worker. Renew and
//! release use the opaque, worker-incarnation-specific control subject returned
//! by reserve. Only reserve and reserved carry a version; control messages use
//! the protocol negotiated by reserve.

pub(super) mod handler;

use super::activation_transport::LocalWorkerTarget;
use serde::{Deserialize, Serialize};

pub const TOOL_RESERVATION_PROTOCOL_VERSION: u32 = 1;
pub const TOOL_RESERVATION_QUEUE_GROUP: &str = "tool-reservation-workers";

pub fn reserve_subject(cluster: &str) -> String {
    format!("cluster.{cluster}.tool_reservations.reserve")
}

pub fn targeted_reserve_subject(target: LocalWorkerTarget<'_>) -> String {
    format!(
        "session_scope.{}.workers.{}.tool_reservations.reserve",
        target.session_scope(),
        target.worker_id()
    )
}

/// Package-relative tool selection, independent of an active agent.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolReservationView {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    pub use_tools: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Reserve {
    pub protocol_version: u32,
    /// Fresh UUID for every attempt, including retries after a lost reply.
    pub attempt_id: String,
    pub session_storage_key: String,
    #[serde(flatten)]
    pub view: ToolReservationView,
}

impl Reserve {
    pub fn new(session_storage_key: impl Into<String>, view: ToolReservationView) -> Self {
        Self {
            protocol_version: TOOL_RESERVATION_PROTOCOL_VERSION,
            attempt_id: uuid::Uuid::new_v4().to_string(),
            session_storage_key: session_storage_key.into(),
            view,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Reserved {
    pub protocol_version: u32,
    pub attempt_id: String,
    pub reservation_id: String,
    pub worker_id: String,
    /// Wire representation of `harnx_core::instance::ServerScope`.
    pub server_scope: String,
    /// Opaque subject; clients must not derive it from the worker ID.
    pub control_subject: String,
    pub ttl_ms: u64,
    pub renew_after_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Renew {
    pub reservation_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Release {
    pub reservation_id: String,
}

/// Discriminates the two operations sent to the same control subject.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolReservationControl {
    Renew(Renew),
    Release(Release),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ToolReservationErrorCode {
    UnknownOrExpired,
    /// Preserve other worker errors without preventing a client from decoding them.
    #[serde(untagged)]
    Other(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolReservationError {
    pub code: ToolReservationErrorCode,
    pub message: String,
}

/// Both success and error replies are plain objects, without a result wrapper.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum ReserveReply {
    Reserved(Reserved),
    Error(ToolReservationError),
}

/// Successful renew/release acknowledgement, serialized as `"Ok"`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ToolReservationOk {
    Ok,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum ToolReservationControlReply {
    Ok(ToolReservationOk),
    Error(ToolReservationError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LOCAL_CLUSTER_KEY;
    use serde::de::DeserializeOwned;
    use serde_json::{json, Value};
    use std::fmt::Debug;

    fn assert_wire_round_trip<T>(message: T, expected: Value)
    where
        T: Serialize + DeserializeOwned + PartialEq + Debug,
    {
        let encoded = serde_json::to_value(&message).unwrap();
        assert_eq!(encoded, expected);
        assert_eq!(serde_json::from_value::<T>(encoded).unwrap(), message);
    }

    #[test]
    fn shared_reserve_subject_and_queue_group() {
        assert_eq!(
            reserve_subject("production"),
            "cluster.production.tool_reservations.reserve"
        );
        assert_eq!(TOOL_RESERVATION_QUEUE_GROUP, "tool-reservation-workers");
    }

    #[test]
    fn local_reserve_subject_uses_validated_target() {
        let target = LocalWorkerTarget::new(LOCAL_CLUSTER_KEY, "worker-42_abc").unwrap();
        assert_eq!(
            targeted_reserve_subject(target),
            "session_scope.__local__.workers.worker-42_abc.tool_reservations.reserve"
        );
        for invalid in ["", "worker.id", "*", ">", "worker id"] {
            assert!(LocalWorkerTarget::new(LOCAL_CLUSTER_KEY, invalid).is_err());
        }
        assert!(LocalWorkerTarget::new("production", "worker-42").is_err());
    }

    #[test]
    fn reserve_and_view_round_trip_with_and_without_package() {
        for package in [None, Some("coding".to_string())] {
            let view = ToolReservationView {
                package: package.clone(),
                use_tools: vec!["fs_*".to_string(), "plans_*".to_string()],
            };
            let mut expected_view = json!({"use_tools": ["fs_*", "plans_*"]});
            if let Some(package) = package {
                expected_view["package"] = json!(package);
            }
            assert_wire_round_trip(view.clone(), expected_view.clone());
            let reserve = Reserve::new("session-key", view);
            assert_eq!(reserve.protocol_version, TOOL_RESERVATION_PROTOCOL_VERSION);
            assert!(uuid::Uuid::parse_str(&reserve.attempt_id).is_ok());
            let mut expected = expected_view;
            expected["protocol_version"] = json!(1);
            expected["attempt_id"] = json!(reserve.attempt_id);
            expected["session_storage_key"] = json!("session-key");
            assert_wire_round_trip(reserve, expected);
        }
    }

    #[test]
    fn reserve_attempts_are_fresh_and_empty_selection_is_valid() {
        let first = Reserve::new("session-key", ToolReservationView::default());
        let second = Reserve::new("session-key", ToolReservationView::default());
        assert_ne!(first.attempt_id, second.attempt_id);
        assert_wire_round_trip(
            first.clone(),
            json!({
                "protocol_version": 1,
                "attempt_id": first.attempt_id,
                "session_storage_key": "session-key",
                "use_tools": []
            }),
        );
    }

    #[test]
    fn protocol_version_and_selectors_are_required() {
        let mut wire = json!({
            "protocol_version": 99,
            "attempt_id": "attempt",
            "session_storage_key": "session-key",
            "use_tools": []
        });
        // Version validation belongs to the handler so it can send an error reply.
        assert_eq!(
            serde_json::from_value::<Reserve>(wire.clone())
                .unwrap()
                .protocol_version,
            99
        );
        wire.as_object_mut().unwrap().remove("protocol_version");
        assert!(serde_json::from_value::<Reserve>(wire.clone()).is_err());
        wire["protocol_version"] = json!(1);
        wire.as_object_mut().unwrap().remove("use_tools");
        assert!(serde_json::from_value::<Reserve>(wire).is_err());
    }

    #[test]
    fn reserved_and_reserve_reply_round_trip() {
        let reserved = Reserved {
            protocol_version: TOOL_RESERVATION_PROTOCOL_VERSION,
            attempt_id: "attempt".to_string(),
            reservation_id: "reservation".to_string(),
            worker_id: "worker-42".to_string(),
            server_scope: "worker-scope".to_string(),
            control_subject: "_INBOX.worker-incarnation".to_string(),
            ttl_ms: 30_000,
            renew_after_ms: 10_000,
        };
        let expected = json!({
            "protocol_version": 1,
            "attempt_id": "attempt",
            "reservation_id": "reservation",
            "worker_id": "worker-42",
            "server_scope": "worker-scope",
            "control_subject": "_INBOX.worker-incarnation",
            "ttl_ms": 30_000,
            "renew_after_ms": 10_000
        });
        assert_wire_round_trip(reserved.clone(), expected.clone());
        assert_wire_round_trip(ReserveReply::Reserved(reserved), expected);
    }

    #[test]
    fn renew_and_release_round_trip_and_control_operations_are_distinct() {
        let renew = Renew {
            reservation_id: "reservation".to_string(),
        };
        let release = Release {
            reservation_id: "reservation".to_string(),
        };
        let payload = json!({"reservation_id": "reservation"});
        assert_wire_round_trip(renew.clone(), payload.clone());
        assert_wire_round_trip(release.clone(), payload);
        assert_wire_round_trip(
            ToolReservationControl::Renew(renew),
            json!({"type": "renew", "reservation_id": "reservation"}),
        );
        assert_wire_round_trip(
            ToolReservationControl::Release(release),
            json!({"type": "release", "reservation_id": "reservation"}),
        );
        assert!(serde_json::from_value::<ToolReservationControl>(
            json!({"type": "unknown", "reservation_id": "reservation"})
        )
        .is_err());
    }

    #[test]
    fn error_replies_round_trip_with_known_and_other_codes() {
        for code in [
            ToolReservationErrorCode::UnknownOrExpired,
            ToolReservationErrorCode::Other("InvalidRequest".to_string()),
        ] {
            let expected_code = match &code {
                ToolReservationErrorCode::UnknownOrExpired => "UnknownOrExpired",
                ToolReservationErrorCode::Other(code) => code,
            };
            let expected = json!({"code": expected_code, "message": "reservation unavailable"});
            let error = ToolReservationError {
                code,
                message: "reservation unavailable".to_string(),
            };
            assert_wire_round_trip(error.clone(), expected.clone());
            assert_wire_round_trip(ReserveReply::Error(error.clone()), expected.clone());
            assert_wire_round_trip(ToolReservationControlReply::Error(error), expected);
        }
    }

    #[test]
    fn successful_control_reply_round_trips() {
        assert_wire_round_trip(ToolReservationOk::Ok, json!("Ok"));
        assert_wire_round_trip(
            ToolReservationControlReply::Ok(ToolReservationOk::Ok),
            json!("Ok"),
        );
    }
}
