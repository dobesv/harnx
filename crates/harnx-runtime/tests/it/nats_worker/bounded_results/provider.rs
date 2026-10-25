use super::*;

pub(super) async fn capture_round() -> Result<CapturedProvider> {
    CapturedProvider::start(
        &[
            ("bridge_large", "bridge-large"),
            ("native_large", "native-large"),
            ("native_sibling", "native-sibling"),
        ],
        "bounded round complete",
    )
    .await
}

pub(super) async fn capture_storage_failure() -> Result<CapturedProvider> {
    CapturedProvider::start(
        &[("native_large", "native-large")],
        "unexpected continuation after storage failure",
    )
    .await
}
