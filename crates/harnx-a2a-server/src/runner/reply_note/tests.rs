use super::*;
use a2a_lf::{Part, Role};
use harnx_runtime::ApprovalRef;

use crate::store::context::{ReplyReceiptParams, NOTE_BYTE_LIMIT};

fn receipt(message: &Message, approved: bool) -> ReplyReceipt {
    ReplyReceipt::new(ReplyReceiptParams::new(
        &message.message_id,
        crate::store::message_fingerprint(&message.parts),
        ApprovalRef {
            invocation_id: "invocation".into(),
            tool_round_seq: 2,
            request_seq: 3,
            tool_call_id: "call".into(),
        },
        approved,
    ))
}

#[test]
fn saved_reply_note_preserves_full_approval_and_rejection_notes() -> Result<()> {
    let note = format!("{}end", "é".repeat(NOTE_BYTE_LIMIT));
    for (word, approved) in [("y", true), ("n", false)] {
        let message = Message::new(Role::User, vec![Part::text(format!("{word} {note}"))]);
        let mut receipt = receipt(&message, approved);
        // Delivery must use history even when the inline note was truncated.
        receipt.note = Some("bounded inline note".into());
        let history = [message];
        assert_eq!(
            saved_reply_note(Some(&history), &receipt)?,
            Some(note.clone())
        );
    }
    Ok(())
}

#[test]
fn saved_reply_note_accepts_decisions_without_notes() -> Result<()> {
    for (word, approved) in [("y", true), ("n", false)] {
        let message = Message::new(Role::User, vec![Part::text(word)]);
        let receipt = receipt(&message, approved);
        assert_eq!(saved_reply_note(Some(&[message]), &receipt)?, None);
    }
    Ok(())
}

#[test]
fn saved_reply_note_rejects_missing_message() {
    let message = Message::new(Role::User, vec![Part::text("y")]);
    let mut receipt = receipt(&message, true);
    receipt.message_id = "missing".into();
    for history in [None, Some([message].as_slice())] {
        assert_eq!(
            saved_reply_note(history, &receipt).unwrap_err().to_string(),
            "accepted reply history missing"
        );
    }
}

#[test]
fn saved_reply_note_rejects_fingerprint_mismatch() {
    let mut message = Message::new(Role::User, vec![Part::text("n original reason")]);
    let receipt = receipt(&message, false);
    message.parts = vec![Part::text("n changed reason")];
    assert_eq!(
        saved_reply_note(Some(&[message]), &receipt)
            .unwrap_err()
            .to_string(),
        "accepted reply history fingerprint mismatch"
    );
}

#[test]
fn saved_reply_note_rejects_opposite_or_unrecognized_decision() {
    let cases = [
        (vec![Part::text("y")], false),
        (vec![Part::text("n")], true),
        (vec![Part::text("yesterday")], true),
        (
            vec![Part::data(serde_json::json!({"approved": true}))],
            true,
        ),
    ];
    for (parts, approved) in cases {
        let message = Message::new(Role::User, parts);
        let receipt = receipt(&message, approved);
        assert_eq!(
            saved_reply_note(Some(&[message]), &receipt)
                .unwrap_err()
                .to_string(),
            "accepted reply history decision mismatch"
        );
    }
}
