//! Actual MCP child -> bridge offload -> worker cache -> OpenAI HTTP request.
use super::result_fixture::{
    BridgeScript, CapturedProvider, McpBridge, ProviderSession, ResultEnvironment, SessionSpec,
};
use super::*;
use serde_json::Value;

#[path = "image_attachments/fixture.rs"]
mod fixture;
#[path = "image_attachments/image.rs"]
mod image;
#[path = "image_attachments/malformed.rs"]
mod malformed;
use fixture::{ImageFixture, RemoteImage};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bridge_image_reaches_same_turn_provider_and_replays_without_redownload() -> Result<()> {
    require_nextest();
    let fixture = ImageFixture::start().await?;
    fixture.run_observed_turn(RemoteImage::Original).await?;
    fixture.image.assert_cached()?;

    // Reconstruct with an empty cache, without calling the tool again.
    std::fs::remove_dir_all(&fixture.image.cache)?;
    fixture.image.assert_cache_missing();
    fixture.session.replay().await?;
    fixture.image.assert_cached()?;

    // Warm replay must use validated local bytes, not the poisoned remote copy.
    fixture.image.corrupt_remote(&fixture.env.js).await?;
    fixture.session.replay().await?;
    fixture.image.assert_cached()?;
    let requests = fixture.provider.finish().await?;
    fixture.image.assert_provider_images(&requests);
    assert_eq!(
        fixture.bridge.call_count()?,
        1,
        "replay must not execute the MCP child again"
    );
    fixture.bridge.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn corrupt_bridge_image_fails_before_same_turn_provider_request() -> Result<()> {
    require_nextest();
    let fixture = ImageFixture::start().await?;
    let error = fixture
        .run_observed_turn(RemoteImage::CorruptBeforeInference)
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("attachment digest mismatch"));
    fixture.image.assert_cache_missing();
    assert_eq!(
        fixture.provider.finish().await?.len(),
        1,
        "corruption must stop before the next provider call"
    );
    assert_eq!(fixture.bridge.call_count()?, 1);
    fixture.bridge.stop().await
}
