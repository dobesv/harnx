use crate::session_actor::SessionKey;
use harnx_core::{interaction::InteractionResponse, session::SessionLogEntry};

/// Conflicts include the durable answer, not an inferred opposite action: two
/// confirmations with the same action but different notes also conflict.
pub(super) async fn response(
    config: &harnx_runtime::config::Config,
    key: &SessionKey,
    interaction_id: &str,
    request_seq: u64,
) -> anyhow::Result<Option<InteractionResponse>> {
    let (_, entries) = crate::load_nats_session(config, key.target(), key.session()).await?;
    let effective = harnx_core::session_reconstruct::apply_log_mutations_nats(&entries)?;
    Ok(effective.into_iter().find_map(|(_, entry)| match entry {
        SessionLogEntry::InteractionResponse {
            interaction_id: id,
            request_seq: seq,
            response,
            ..
        } if id == interaction_id && seq == request_seq => Some(response),
        _ => None,
    }))
}
