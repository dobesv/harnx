//! Preserve protocol error codes while keeping broker details out of responses.
use crate::{input_map::InputMapError, runner::RunnerError, store::StoreError};
use a2a_lf::A2AError;

/// Server-defined permission error, outside A2A's -32001..-32009 codes.
pub const PERMISSION_DENIED_CODE: i32 = -32010;

pub(super) fn permission_denied() -> A2AError {
    A2AError::new(
        PERMISSION_DENIED_CODE,
        "session creation requires prompt scope",
    )
}

pub(super) fn not_found() -> A2AError {
    A2AError::new(-32001, "task not found")
}

// qual:allow(coupling, deh) reason: "Typed runtime failures ride the anyhow boundary; matching them by downcast is the house pattern for typed sources on anyhow errors."
pub(super) fn map_error(error: anyhow::Error) -> A2AError {
    if let Some(error) = error.downcast_ref::<StoreError>() {
        return store_error(*error);
    }
    if let Some(error) = error.downcast_ref::<InputMapError>() {
        return input_error(error);
    }
    if let Some(error) = error.downcast_ref::<RunnerError>() {
        return runner_error(*error);
    }
    if error
        .downcast_ref::<harnx_runtime::nats_session_metadata::SessionBusy>()
        .is_some()
    {
        // The runtime admission head names other live work (including a
        // steered drain that has not finished): the honest answer is the
        // retryable busy error, not a generic failure.
        return runner_error(RunnerError::Busy);
    }
    if error
        .downcast_ref::<harnx_runtime::nats_session::SteeredContentMismatch>()
        .is_some()
    {
        // A steering replay found a committed row for this messageId with
        // different content — the same violation the dedupe mapping's
        // fingerprint check reports, surfaced by the runtime only when the
        // mapping write failed or never ran (the documented crash window).
        return store_error(StoreError::FingerprintMismatch);
    }
    tracing::warn!(%error, "A2A request failed");
    A2AError::internal("request failed")
}

fn store_error(error: StoreError) -> A2AError {
    match error {
        StoreError::NotFound => not_found(),
        StoreError::FingerprintMismatch => {
            A2AError::invalid_params("messageId was already used with different parts")
        }
    }
}

fn input_error(error: &InputMapError) -> A2AError {
    match error {
        InputMapError::UnsupportedMediaType { media_type } => {
            let media_type: String = media_type
                .chars()
                .filter(|character| !character.is_control())
                .take(128)
                .collect();
            A2AError::new(
                a2a_lf::error_code::CONTENT_TYPE_NOT_SUPPORTED,
                format!("unsupported raw mediaType: {media_type}"),
            )
        }
        InputMapError::DataPartTooLarge { .. }
        | InputMapError::InvalidUtf8 { .. }
        | InputMapError::JsonEncoding(_) => {
            A2AError::invalid_params("unsupported or oversized message parts")
        }
    }
}

fn runner_error(error: RunnerError) -> A2AError {
    match error {
        RunnerError::Busy => {
            A2AError::new(-32000, "context already has an active task, retry later")
        }
        RunnerError::Terminal => A2AError::invalid_params("task is terminal"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_runtime::nats_session_metadata::SessionBusy;

    /// A `SessionBusy` from the runtime admission head — including one raised
    /// while a steered drain is still finishing — must surface as the
    /// retryable busy error, never as an opaque internal error. Callers wrap
    /// runtime errors in context, so the wrapped form is asserted too.
    #[test]
    fn session_busy_maps_to_the_retryable_busy_error() {
        for error in [
            SessionBusy::ActiveInvocation {
                invocation_id: "inv-1".to_owned(),
            },
            SessionBusy::ReservationKeptChanging,
        ] {
            let wrapped =
                anyhow::Error::new(error.clone()).context("admission head still draining");
            for mapped in [map_error(error.into()), map_error(wrapped)] {
                assert_eq!(mapped.code, -32000, "{mapped:?}");
                assert_eq!(
                    mapped.message,
                    "context already has an active task, retry later"
                );
            }
        }
    }
}
