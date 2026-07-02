use super::*;
use crate::nats_client_session::{ThinClientConfig, ThinClientSession};
use crate::nats_session_log::NatsSessionLog;
use crate::utils::{edit_file, temp_file, AbortSignal};
use anyhow::{anyhow, bail, Context, Result};
use harnx_core::session_reconstruct::active_context_window;

impl Config {
    pub(crate) fn edit_message_text_with_tui_hooks(
        &mut self,
        initial_text: &str,
    ) -> Result<String> {
        let temp_file = if let Some(ref dir) = self.temp_dir_override {
            dir.join(format!("message-edit-{}.txt", uuid::Uuid::new_v4()))
        } else {
            temp_file("message-edit", ".txt")
        };

        std::fs::write(&temp_file, initial_text)
            .with_context(|| format!("Failed to write to '{}'", temp_file.display()))?;

        let edit_result = self.edit_with_tui_hooks(|this| {
            let editor = this.editor()?;
            edit_file(&editor, &temp_file).with_context(|| {
                format!("Failed to edit '{}' with '{}'", temp_file.display(), editor)
            })
        });
        let edited_content = std::fs::read_to_string(&temp_file)
            .with_context(|| format!("Failed to read '{}'", temp_file.display()));
        let _ = std::fs::remove_file(&temp_file);
        edit_result?;
        edited_content
    }
}

pub(crate) async fn edit_remote_message_range(
    config: &GlobalConfig,
    from: usize,
    to: usize,
    abort_signal: &AbortSignal,
) -> Result<()> {
    let thin = remote_thin_session(config, abort_signal).await?;

    let selected_messages = remote_user_messages_for_range(from, to, &thin).await?;
    if selected_messages.len() > 1 {
        bail!("Remote edit supports a single user message at a time");
    }
    let target = selected_messages
        .into_iter()
        .next()
        .context("Selected range does not contain any user messages")?;

    let new_text = {
        let mut cfg = config.write();
        cfg.edit_message_text_with_tui_hooks(&target.text)?
    };

    append_session_mutation_batch_cas(&thin, |state| {
        let target_logical_index = find_target_logical_index(state, target.js_seq)?;
        let group = load_seq_group(state, target.js_seq)?;
        let replacements = build_edit_replacements(group, target_logical_index, &new_text)?;
        build_seq_edit_mutation(target.js_seq, replacements)
    })
    .await?;

    Ok(())
}

fn find_target_logical_index(state: &RemoteRenderState, target_seq: u64) -> Result<usize> {
    state
        .logical_targets
        .iter()
        .enumerate()
        .find(|(_, target)| {
            target.js_seq == target_seq
                && matches!(
                    &target.entry,
                    SessionLogEntry::Message { role, .. } if role.is_user()
                )
        })
        .map(|(index, _)| index)
        .context("target user message not found in logical targets")
}

fn load_seq_group(state: &RemoteRenderState, target_seq: u64) -> Result<Vec<(usize, String)>> {
    let seq = usize::try_from(target_seq).context("JetStream seq does not fit into usize")?;
    let mut group = state
        .logical_targets
        .iter()
        .enumerate()
        .filter(|(_, target)| target.js_seq == target_seq)
        .map(|(index, target)| {
            let yaml = serde_yaml::to_string(&target.entry)
                .context("failed to serialize remote group member")?;
            Ok((index, yaml))
        })
        .collect::<Result<Vec<_>>>()?;
    group.sort_by_key(|(index, _)| *index);
    verify_contiguous_group(seq, &group)?;
    Ok(group)
}

fn build_edit_replacements(
    group: Vec<(usize, String)>,
    target_logical_index: usize,
    new_text: &str,
) -> Result<Vec<String>> {
    if group.len() == 1 {
        return Ok(vec![serialize_edited_user_entry(new_text)?]);
    }

    let edited_yaml = serialize_edited_user_entry(new_text)?;
    Ok(group
        .into_iter()
        .map(|(logical_index, yaml)| {
            if logical_index == target_logical_index {
                edited_yaml.clone()
            } else {
                yaml
            }
        })
        .collect())
}

fn serialize_edited_user_entry(new_text: &str) -> Result<String> {
    let edited_entry = SessionLogEntry::Message {
        id: Some(uuid::Uuid::new_v4().to_string()),
        role: harnx_core::message::MessageRole::User,
        content: harnx_core::message::MessageContent::Text(new_text.to_string()),
        timestamp: None,
        fence_token: None,
    };
    serde_yaml::to_string(&edited_entry).context("failed to serialize edited entry")
}

fn build_seq_edit_mutation(
    target_seq: u64,
    replacements: Vec<String>,
) -> Result<Vec<SessionLogEntry>> {
    let seq = usize::try_from(target_seq).context("JetStream seq does not fit into usize")?;
    Ok(vec![SessionLogEntry::EditEntries {
        from: seq,
        to: seq,
        replacements,
    }])
}

pub(crate) async fn delete_remote_message_range(
    config: &GlobalConfig,
    from: usize,
    to: usize,
    abort_signal: &AbortSignal,
) -> Result<()> {
    let thin = remote_thin_session(config, abort_signal).await?;
    append_session_mutation_batch_cas(&thin, |state| {
        let (from, to) = self::session_ops_core::compute_delete_range(
            from,
            to,
            &state.logical_entries,
            &state.logical_documents,
        )?;
        // Build the set of targeted logical indices
        let targeted_logical_indices: std::collections::HashSet<usize> = (from..=to).collect();
        // Use group-aware deletions to preserve siblings in shared-seq groups
        group_aware_delete_mutations(state, targeted_logical_indices)
    })
    .await?;
    Ok(())
}

/// Build group-aware deletion mutations: for each distinct physical seq touched,
/// emit `EditEntries{seq,seq}` with `replacements` = surviving group members (in order).
/// This preserves siblings in shared-seq groups (e.g. Header+U1 after header-insert migration)
/// while deleting exactly the targeted logical rows.
fn group_aware_delete_mutations(
    state: &RemoteRenderState,
    targeted_logical_indices: std::collections::HashSet<usize>,
) -> Result<Vec<SessionLogEntry>> {
    if targeted_logical_indices.is_empty() {
        return Ok(Vec::new());
    }

    let groups = build_logical_target_groups(state)?;
    let targeted_seqs = targeted_group_seqs(state, &targeted_logical_indices)?;

    targeted_seqs
        .into_iter()
        .map(|seq| build_group_delete_mutation(seq, &groups, &targeted_logical_indices))
        .collect()
}

fn build_logical_target_groups(
    state: &RemoteRenderState,
) -> Result<std::collections::BTreeMap<usize, Vec<(usize, String)>>> {
    let mut groups = std::collections::BTreeMap::new();
    for (logical_index, target) in state.logical_targets.iter().enumerate() {
        let seq =
            usize::try_from(target.js_seq).context("JetStream seq does not fit into usize")?;
        let yaml = serde_yaml::to_string(&target.entry)
            .context("failed to serialize remote logical target")?;
        groups
            .entry(seq)
            .or_insert_with(Vec::new)
            .push((logical_index, yaml));
    }
    for (seq, members) in &groups {
        verify_contiguous_group(*seq, members)?;
    }
    Ok(groups)
}

fn targeted_group_seqs(
    state: &RemoteRenderState,
    targeted_logical_indices: &std::collections::HashSet<usize>,
) -> Result<Vec<usize>> {
    let mut targeted_seqs: Vec<usize> = state
        .logical_targets
        .iter()
        .enumerate()
        .filter(|(logical_index, _)| targeted_logical_indices.contains(logical_index))
        .map(|(_, target)| {
            usize::try_from(target.js_seq).context("JetStream seq does not fit into usize")
        })
        .collect::<Result<Vec<_>>>()?;
    targeted_seqs.sort_unstable();
    targeted_seqs.dedup();
    Ok(targeted_seqs)
}

fn build_group_delete_mutation(
    seq: usize,
    groups: &std::collections::BTreeMap<usize, Vec<(usize, String)>>,
    targeted_logical_indices: &std::collections::HashSet<usize>,
) -> Result<SessionLogEntry> {
    let group_members = groups
        .get(&seq)
        .context("physical seq not found in groups")?;
    let survivors = survivor_group_members(group_members, targeted_logical_indices);
    Ok(SessionLogEntry::EditEntries {
        from: seq,
        to: seq,
        replacements: survivors,
    })
}

fn survivor_group_members(
    group_members: &[(usize, String)],
    targeted_logical_indices: &std::collections::HashSet<usize>,
) -> Vec<String> {
    group_members
        .iter()
        .filter(|(logical_index, _)| !targeted_logical_indices.contains(logical_index))
        .map(|(_, entry_yaml)| entry_yaml.clone())
        .collect()
}

fn verify_contiguous_group(seq: usize, members: &[(usize, String)]) -> Result<()> {
    let first_idx = members.first().map(|(index, _)| *index);
    let last_idx = members.last().map(|(index, _)| *index);
    if let (Some(first), Some(last)) = (first_idx, last_idx) {
        let expected_len = last.saturating_sub(first) + 1;
        if members.len() != expected_len {
            bail!(
                "shared-seq group members are non-contiguous: seq {} spans logical indices {:?} (gap in middle unsupported)",
                seq,
                members.iter().map(|(index, _)| *index).collect::<Vec<_>>()
            );
        }
    }
    Ok(())
}

pub(crate) async fn rewind_remote_session(
    config: &GlobalConfig,
    after_seq: usize,
    abort_signal: &AbortSignal,
) -> Result<()> {
    let thin = remote_thin_session(config, abort_signal).await?;
    append_session_mutation_batch_cas(&thin, |state| {
        let len = state.logical_entries.len();
        let after_seq =
            self::session_ops_core::compute_rewind_point(after_seq, len, &state.logical_entries)?;
        // A logical rewind drops the exact logical SUFFIX (every logical entry
        // after the cutoff). Use group-aware deletions to preserve any siblings
        // in shared-seq groups that are in the kept prefix.
        let suffix_logical_indices: std::collections::HashSet<usize> =
            (after_seq + 1..len).collect();
        group_aware_delete_mutations(state, suffix_logical_indices)
    })
    .await?;
    Ok(())
}

async fn remote_thin_session(
    config: &GlobalConfig,
    abort_signal: &AbortSignal,
) -> Result<ThinClientSession> {
    let (agent, cluster, session_id) = {
        let cfg = config.read();
        let (agent, cluster) = cfg.remote_agent.clone().context("No remote agent")?;
        let session_id = cfg
            .session
            .as_ref()
            .map(|session| session.id().to_string())
            .context("No session")?;
        (agent, cluster, session_id)
    };

    ThinClientSession::from_global_config(
        ThinClientConfig {
            cluster,
            agent,
            session_id: Some(session_id),
        },
        config,
        abort_signal.clone(),
    )
    .await
}

pub struct RemoteTranscriptState {
    pub compressed_messages: Vec<harnx_core::message::Message>,
    pub messages: Vec<harnx_core::message::Message>,
}

fn renumber_remote_messages_for_window(
    messages: &mut [harnx_core::message::Message],
    _effective_entries: &[(u64, SessionLogEntry)],
    window: &harnx_core::session_reconstruct::ActiveContextWindow<'_, (u64, SessionLogEntry)>,
) {
    let mut available_members = build_window_member_queues(window);
    for message in messages.iter_mut() {
        let Some(seq) = message.log_seq.and_then(|seq| u64::try_from(seq).ok()) else {
            message.log_seq = None;
            continue;
        };
        let Some(member) = consume_window_member(&mut available_members, seq, message.role) else {
            message.log_seq = None;
            continue;
        };
        message.log_seq = Some(member.logical_index);
    }
}

#[derive(Clone, Copy)]
struct WindowMemberAssignment {
    logical_index: usize,
    role: harnx_core::message::MessageRole,
}

fn build_window_member_queues(
    window: &harnx_core::session_reconstruct::ActiveContextWindow<'_, (u64, SessionLogEntry)>,
) -> std::collections::HashMap<u64, std::collections::VecDeque<WindowMemberAssignment>> {
    // Pair each active-window entry with its OWN logical index (window.entries()
    // and window.logical_entries() are in the same order). For a header-insert
    // shared-physical-seq group [Header, U1, U2, ...] every member has a distinct
    // logical index (Header=0, U1=1, ...). Queue the renderable (non-Header)
    // members per physical seq, in order, each with its own logical index — so a
    // transcript row sharing that seq is assigned the NEXT member's index rather
    // than collapsing to the group's first/last. The Header renders no transcript
    // row, so it is never queued/consumed.
    let mut available_members: std::collections::HashMap<
        u64,
        std::collections::VecDeque<WindowMemberAssignment>,
    > = std::collections::HashMap::new();
    for ((physical_seq, entry), logical) in window.entries().iter().zip(window.logical_entries()) {
        if matches!(entry, SessionLogEntry::Header { .. }) {
            continue;
        }
        let Some(role) = message_role_for_assignment(entry) else {
            continue;
        };
        available_members
            .entry(*physical_seq)
            .or_default()
            .push_back(WindowMemberAssignment {
                logical_index: logical.logical_index,
                role,
            });
    }
    available_members
}

fn message_role_for_assignment(
    entry: &SessionLogEntry,
) -> Option<harnx_core::message::MessageRole> {
    match entry {
        SessionLogEntry::Message { role, .. } => Some(*role),
        _ => None,
    }
}

fn consume_window_member(
    available_members: &mut std::collections::HashMap<
        u64,
        std::collections::VecDeque<WindowMemberAssignment>,
    >,
    seq: u64,
    role: harnx_core::message::MessageRole,
) -> Option<WindowMemberAssignment> {
    let queue = available_members.get_mut(&seq)?;
    while let Some(member) = queue.pop_front() {
        if member.role == role {
            return Some(member);
        }
    }
    None
}

pub async fn load_remote_transcript_for_render(
    thin: &ThinClientSession,
) -> Result<RemoteTranscriptState> {
    let log = NatsSessionLog::new(thin.jetstream().clone(), thin.session_id().to_string());
    let entries = log.load_events_async().await?;
    let effective_entries = harnx_core::session_reconstruct::apply_log_mutations_nats(&entries)
        .context("failed to reconstruct remote session log for transcript render")?;
    let window = active_context_window(&effective_entries);

    if window.boundary_index().is_none() {
        return Ok(RemoteTranscriptState {
            compressed_messages: vec![],
            messages: vec![],
        });
    }

    let raw_entries = effective_entries
        .iter()
        .map(|(seq, entry)| {
            Ok((
                usize::try_from(*seq).context("JetStream seq does not fit into usize")?,
                entry.clone(),
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut session =
        super::session::replay_log_entries_for_external(&raw_entries, thin.session_id())?;

    renumber_remote_messages_for_window(
        &mut session.compressed_messages,
        &effective_entries,
        &window,
    );
    renumber_remote_messages_for_window(&mut session.messages, &effective_entries, &window);

    Ok(RemoteTranscriptState {
        compressed_messages: session.compressed_messages,
        messages: session.messages,
    })
}

struct RemoteUserMessageSelection {
    js_seq: u64,
    text: String,
}

async fn remote_user_messages_for_range(
    from: usize,
    to: usize,
    thin: &ThinClientSession,
) -> Result<Vec<RemoteUserMessageSelection>> {
    let state = load_remote_session_for_render(thin).await?;
    let mut user_messages = Vec::new();
    for logical_index in from..=to {
        let target = state.logical_target(logical_index)?;
        if let SessionLogEntry::Message { role, content, .. } = &target.entry {
            if role.is_user() {
                let text = match content {
                    harnx_core::message::MessageContent::Text(text) => text.clone(),
                    _ => bail!("Remote edit supports only text user messages"),
                };
                user_messages.push(RemoteUserMessageSelection {
                    js_seq: target.js_seq,
                    text,
                });
            }
        }
    }

    if user_messages.is_empty() {
        bail!("Selected range does not contain any user messages")
    }

    Ok(user_messages)
}

async fn append_session_mutation_batch_cas<F>(
    thin: &ThinClientSession,
    build_entries: F,
) -> Result<()>
where
    F: Fn(&RemoteRenderState) -> Result<Vec<SessionLogEntry>>,
{
    const MAX_CAS_ATTEMPTS: usize = 10;
    let log = NatsSessionLog::new(thin.jetstream().clone(), thin.session_id().to_string());
    for attempt in 1..=MAX_CAS_ATTEMPTS {
        let state = load_remote_session_for_render(thin).await?;
        let entries = build_entries(&state)?;
        if entries.is_empty() {
            return Ok(());
        }
        let mut expected_last = state.last_seen_stream_seq;
        let mut should_retry = false;
        for entry in &entries {
            match log
                .append_event_with_expected_last_sequence_async(entry, expected_last)
                .await
            {
                Ok(seq) => expected_last = seq,
                Err(err) if is_stream_advanced_error(&err) => {
                    should_retry = true;
                    break;
                }
                Err(err) => return Err(err),
            }
        }
        if !should_retry {
            return Ok(());
        }
        if attempt == MAX_CAS_ATTEMPTS {
            break;
        }
        tokio::time::sleep(cas_retry_delay(attempt)).await;
    }
    Err(anyhow!(
        "remote session mutation failed after {MAX_CAS_ATTEMPTS} attempts due to concurrent writes"
    ))
}

fn cas_retry_delay(attempt: usize) -> std::time::Duration {
    let millis = 5_u64.saturating_mul(attempt as u64);
    std::time::Duration::from_millis(millis)
}

fn is_stream_advanced_error(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause.to_string().contains("wrong last sequence")
            || cause.to_string().contains("expected last sequence")
            || cause.to_string().contains("stream sequence does not match")
    })
}

pub(crate) struct RemoteRenderState {
    pub(crate) logical_entries: Vec<SessionLogEntry>,
    pub(crate) logical_documents: Vec<String>,
    logical_targets: Vec<RemoteLogicalTarget>,
    pub(crate) last_seen_stream_seq: u64,
}

impl RemoteRenderState {
    fn logical_target(&self, logical_index: usize) -> Result<&RemoteLogicalTarget> {
        self.logical_targets
            .get(logical_index)
            .context("Sequence numbers out of range")
    }

    #[allow(dead_code)]
    fn logical_index_to_js_seq(&self, logical_index: usize) -> Result<usize> {
        let js_seq = self.logical_target(logical_index)?.js_seq;
        usize::try_from(js_seq).context("JetStream seq does not fit into usize")
    }

    #[allow(dead_code)]
    fn logical_range_to_targeted_js_seqs(&self, from: usize, to: usize) -> Result<Vec<usize>> {
        let mut js_seqs = Vec::with_capacity(to.saturating_sub(from) + 1);
        for logical_index in from..=to {
            js_seqs.push(self.logical_index_to_js_seq(logical_index)?);
        }
        Ok(js_seqs)
    }
}

struct RemoteLogicalTarget {
    js_seq: u64,
    entry: SessionLogEntry,
}

pub(crate) async fn load_remote_session_for_render(
    thin: &ThinClientSession,
) -> Result<RemoteRenderState> {
    let log = NatsSessionLog::new(thin.jetstream().clone(), thin.session_id().to_string());
    let entries = log.load_events_async().await?;
    let rendered = harnx_core::session_reconstruct::apply_log_mutations_nats(&entries)
        .context("failed to reconstruct remote session log before edit/delete")?;

    let last_seen_stream_seq = entries.last().map(|(seq, _)| *seq).unwrap_or(0);
    let logical_window = active_context_window(&rendered);
    let logical_targets = logical_window
        .entries()
        .iter()
        .zip(logical_window.logical_entries())
        .map(|((_, entry), logical_entry)| RemoteLogicalTarget {
            js_seq: logical_entry.physical_seq,
            entry: entry.clone(),
        })
        .collect::<Vec<_>>();
    let logical_entries = logical_targets
        .iter()
        .map(|target| target.entry.clone())
        .collect::<Vec<_>>();
    let logical_documents = logical_entries
        .iter()
        .map(serde_yaml::to_string)
        .collect::<std::result::Result<Vec<_>, _>>()?;

    Ok(RemoteRenderState {
        logical_entries,
        logical_documents,
        logical_targets,
        last_seen_stream_seq,
    })
}
