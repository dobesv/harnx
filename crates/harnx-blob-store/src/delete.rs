//! Owner deletion: remove all media and plans for a session owner.

use anyhow::Result;

use crate::media::delete_media_prefix;
use crate::plans::delete_plans_prefix;

/// Delete all data for an owner.
///
/// Removes:
/// - `media/<owner>/` objects from the attachments bucket
/// - `plan/<owner>/` keys from the plans KV bucket
///
/// Returns the total number of items deleted.
pub async fn delete_owner(
    jetstream: &async_nats::jetstream::Context,
    owner: &str,
) -> Result<usize> {
    let media_count = delete_media_prefix(jetstream, owner).await?;
    let plans_count = delete_plans_prefix(jetstream, owner).await?;
    Ok(media_count + plans_count)
}
