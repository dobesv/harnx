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
