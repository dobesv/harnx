//! Full saved reply notes, validated before immediate or recovered delivery.
use a2a_lf::Message;
use anyhow::{Context, Result};

#[cfg(test)]
use crate::hitl::{parse_reply, ReplyParse};
use crate::store::context::ReplyReceipt;

#[cfg(test)]
pub(super) fn saved_reply_note(
    history: Option<&[Message]>,
    receipt: &ReplyReceipt,
) -> Result<Option<String>> {
    let saved = saved_reply_message(history, receipt)?;
    match parse_reply(&saved.parts) {
        ReplyParse::Approve(note) if receipt.approved => Ok(note),
        ReplyParse::Reject(note) if !receipt.approved => Ok(note),
        _ => anyhow::bail!("accepted reply history decision mismatch"),
    }
}

pub(super) fn saved_reply_message<'a>(
    history: Option<&'a [Message]>,
    receipt: &ReplyReceipt,
) -> Result<&'a Message> {
    let saved = history
        .and_then(|history| history.iter().find(|m| m.message_id == receipt.message_id))
        .context("accepted reply history missing")?;
    anyhow::ensure!(
        crate::store::message_fingerprint(&saved.parts) == receipt.fingerprint,
        "accepted reply history fingerprint mismatch"
    );
    Ok(saved)
}

#[cfg(test)]
mod tests;
