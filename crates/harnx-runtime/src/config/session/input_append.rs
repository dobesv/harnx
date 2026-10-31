//! Persist turn input while keeping durable and in-memory message identities aligned.

use super::*;
use crate::config::session_externalize::{externalize_content, record_externalized};

/// Persist input before model work. Cancellation may reject every output from
/// this round, but cannot undo its input. NATS inputs retain their admission's
/// `skip_user_log_append`; they are already generation-owned durable history.
pub(crate) fn prepare_input(session: &mut Session, input: &mut Input) -> Result<()> {
    if input.session_input_start.is_some() {
        return Ok(());
    }
    if input.continue_output().is_some() || input.regenerate() {
        return Ok(());
    }
    let history_len = session.messages.len();
    let mut unpatched = input.clone();
    unpatched.clear_patch();
    let all_appended = begin_turn(session, &unpatched, "")?;
    session.dirty |= !all_appended;
    require_authoritative_appends(session, all_appended, "user input")?;
    input.session_input_start = Some(history_len);
    session.update_tokens();
    Ok(())
}

pub(super) fn begin_turn(session: &mut Session, input: &Input, _output: &str) -> Result<bool> {
    if input.session_input_start.is_some() {
        return Ok(true);
    }
    let mut all_appended = true;
    let is_continuation = is_tool_continuation(input);

    if session.messages.is_empty() && !is_continuation {
        all_appended &= append_initial_agent_messages(session, input)?;
    } else if !is_continuation && !input.skip_user_log_append {
        // `skip_user_log_append` is set by the NATS HA worker: the user
        // message is already durably logged by the client and present in the
        // loaded `session.messages`, so re-appending it would duplicate it and
        // reorder the assistant barrier past concurrent arrivals.
        all_appended &= append_user_turn_message(session, input);
    }

    all_appended &= append_input_data_urls(session, input);
    all_appended &= append_injected_user_text(session, input);
    Ok(all_appended)
}

fn append_initial_agent_messages(session: &mut Session, input: &Input) -> Result<bool> {
    let agent_messages = input.agent().build_messages(input)?;
    let mut all_appended = true;
    let mut cid_urls = std::collections::HashMap::new();

    for mut msg in agent_messages {
        if msg.role == MessageRole::System {
            continue;
        }
        // `externalize_content` only writes files + rewrites `content` in
        // memory (takes `&Session`, cannot append), so `seq` stays correct.
        let seq = session.next_seq();
        cid_urls.extend(externalize_content(session, &mut msg.content));
        let id = persisted_message_id();
        all_appended &=
            append_session_message_with_id(session, id.clone(), msg.role, msg.content.clone());
        session.messages.push(msg.with_id(id).with_log_seq(seq));
    }

    // Seq-less metadata record for the cids written above; appended after
    // the messages so message log_seqs are unaffected.
    all_appended &= record_externalized(session, cid_urls);
    Ok(all_appended)
}

fn append_user_turn_message(session: &mut Session, input: &Input) -> bool {
    // `seq` is the message's log-document index. `externalize_content` only
    // writes attachment files + rewrites `content` in memory (it takes
    // `&Session`, so it cannot append), so nothing must be appended between
    // here and the Message append below or `seq` would be off by one. The
    // cid DataUrls entry is a seq-less metadata record appended *after* the
    // message.
    let seq = session.next_seq();
    let mut content = input.message_content();
    let cid_urls = externalize_content(session, &mut content);
    // One id for both the durable row and the in-memory push, so a compaction
    // re-log of the memory message copies the durable row's identity instead
    // of writing an ID-less duplicate.
    let user_msg_id = persisted_message_id();
    let user_msg = Message::new(MessageRole::User, content)
        .with_id(user_msg_id.clone())
        .with_log_seq(seq);
    let mut all_appended = append_session_message_with_id(
        session,
        user_msg_id,
        user_msg.role,
        user_msg.content.clone(),
    );
    session.messages.push(user_msg);
    emit_agent_event(AgentEvent::Session(SessionEvent::LogSeqAssigned { seq }));
    all_appended &= record_externalized(session, cid_urls);
    all_appended
}

fn append_input_data_urls(session: &mut Session, input: &Input) -> bool {
    let new_data_urls = input.data_urls();
    let mut all_appended = true;
    if !new_data_urls.is_empty() {
        all_appended &= append_event(
            session,
            &SessionLogEntry::DataUrls {
                urls: new_data_urls.clone(),
            },
        );
    }
    session.data_urls.extend(new_data_urls);
    all_appended
}

fn append_injected_user_text(session: &mut Session, input: &Input) -> bool {
    let Some(injected) = input.injected_user_text() else {
        return true;
    };

    // `skip_user_log_append` marks an input whose user text was folded out of
    // the durable log, so the log already holds this message and re-appending
    // it is not just a duplicate row: the worker's mid-round fold would read
    // the fresh copy as another unanswered message and inject it again, once
    // more per remaining tool round. The in-memory push still has to happen —
    // `session.messages` is loaded once per turn and is what carries the
    // injected message into the following rounds' wire requests.
    if input.skip_user_log_append {
        // The NATS worker's mid-turn callback selected durable, admitted user
        // rows. Push those rows verbatim — id and log seq intact — instead of a
        // fresh ID-less message: a later compaction re-logs them under their
        // original identity, so they stay deduplicated copies of admitted
        // prompts rather than becoming unadmitted ID-less user rows (issue
        // #2493's "injected user row has no admission identity").
        restore_injected_user_messages(session, input, injected);
        return true;
    }

    let seq = session.next_seq();
    let id = persisted_message_id();
    let injected_msg = Message::new(
        MessageRole::User,
        MessageContent::Text(injected.to_string()),
    )
    .with_id(id.clone())
    .with_log_seq(seq);
    let all_appended = append_session_message_with_id(
        session,
        id,
        injected_msg.role,
        injected_msg.content.clone(),
    );
    session.messages.push(injected_msg);
    emit_agent_event(AgentEvent::Session(SessionEvent::LogSeqAssigned { seq }));
    all_appended
}

fn restore_injected_user_messages(session: &mut Session, input: &Input, injected: &str) {
    if input.injected_user_messages.is_empty() {
        // Text-only injection has no durable provenance to restore.
        session.messages.push(Message::new(
            MessageRole::User,
            MessageContent::Text(injected.to_string()),
        ));
    } else {
        session
            .messages
            .extend(input.injected_user_messages.iter().cloned());
    }
}

pub(super) fn append_session_message(
    session: &mut Session,
    role: MessageRole,
    content: MessageContent,
) -> bool {
    append_session_message_with_id(session, persisted_message_id(), role, content)
}

/// Like [`append_session_message`] but with a caller-chosen id, so the durable
/// row and the in-memory message pushed alongside it share one identity. A
/// compaction re-log of that in-memory message then copies the durable row's
/// id instead of manufacturing a duplicate ID-less row.
fn append_session_message_with_id(
    session: &mut Session,
    id: String,
    role: MessageRole,
    content: MessageContent,
) -> bool {
    append_event(
        session,
        &SessionLogEntry::Message {
            id: Some(id),
            role,
            content,
            timestamp: Some(Utc::now()),
            fence_token: None,
        },
    )
}
