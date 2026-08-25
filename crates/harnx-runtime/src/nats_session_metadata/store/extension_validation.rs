use super::*;
use anyhow::Result;
use serde_json::Value;
use std::collections::BTreeMap;

pub(super) fn validate_namespace(namespace: &str) -> Result<()> {
    anyhow::ensure!(
        !namespace.is_empty(),
        "extension namespace must not be empty"
    );
    anyhow::ensure!(
        namespace.len() <= 128
            && namespace
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')),
        "invalid extension namespace '{namespace}'"
    );
    Ok(())
}

pub(in crate::nats_session_metadata) fn validate_extensions(
    extensions: &BTreeMap<String, Value>,
) -> Result<()> {
    for (namespace, value) in extensions {
        validate_namespace(namespace)?;
        anyhow::ensure!(
            serde_json::to_vec(value)?.len() <= EXTENSION_NAMESPACE_MAX_BYTES,
            "extension namespace '{namespace}' exceeds {} bytes",
            EXTENSION_NAMESPACE_MAX_BYTES
        );
    }
    anyhow::ensure!(
        serde_json::to_vec(extensions)?.len() <= EXTENSIONS_TOTAL_MAX_BYTES,
        "all session extensions exceed {} bytes",
        EXTENSIONS_TOTAL_MAX_BYTES
    );
    Ok(())
}
