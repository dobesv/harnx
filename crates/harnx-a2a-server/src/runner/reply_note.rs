//! Full saved reply notes, validated before immediate or recovered delivery.
use a2a_lf::Message;
use anyhow::{Context, Result};

use crate::{
    hitl::{parse_reply, ReplyParse},
    store::context::ReplyReceipt,
};

pub(super) fn saved_reply_note(
    history: Option<&[Message]>,
    receipt: &ReplyReceipt,
) -> Result<Option<String>> {
    let saved = history
        .and_then(|history| history.iter().find(|m| m.message_id == receipt.message_id))
        .context("accepted reply history missing")?;
    anyhow::ensure!(
        crate::store::message_fingerprint(&saved.parts) == receipt.fingerprint,
        "accepted reply history fingerprint mismatch"
    );
    match parse_reply(&saved.parts) {
        ReplyParse::Approve(note) if receipt.approved => Ok(note),
        ReplyParse::Reject(note) if !receipt.approved => Ok(note),
        _ => anyhow::bail!("accepted reply history decision mismatch"),
    }
}

#[cfg(test)]
mod tests;
