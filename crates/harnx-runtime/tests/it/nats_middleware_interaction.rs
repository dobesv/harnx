//! Live suspension integration tests for in-process tool middleware interactions across NATS.

#[path = "nats_middleware_interaction/harness.rs"]
pub(crate) mod harness;

#[path = "nats_middleware_interaction/suspension_cases.rs"]
mod suspension_cases;

#[path = "nats_middleware_interaction/save_restore_cases.rs"]
mod save_restore_cases;

#[path = "nats_middleware_interaction/dispatch_cases.rs"]
mod dispatch_cases;

#[path = "nats_middleware_interaction/session_wait_cases.rs"]
mod session_wait_cases;

#[path = "nats_middleware_interaction/decision_receipt_cases.rs"]
mod decision_receipt_cases;

#[path = "nats_middleware_interaction/clear_cases.rs"]
mod clear_cases;

#[path = "nats_middleware_interaction/tool_input_required_cases.rs"]
mod tool_input_required_cases;
