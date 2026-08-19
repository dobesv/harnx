use crate::message::{Message, MessageRole};
use crate::session::{SessionLogEntry, ToolOutput};
use crate::tool::ToolCall;
use anyhow::{bail, Result};

/// Apply mutation entries (EditEntries, Rewind) to build the effective entry stream.
///
/// This is the canonical resolver for mutation semantics:
/// - `EditEntries { from, to, replacements: [] }` deletes entries in [from, to]
/// - `EditEntries { from, to, replacements: [yaml, ...] }` replaces with parsed entries
/// - `Rewind { after_seq }` truncates effective entries to <= after_seq
///
/// Invalid/malformed mutations are skipped with warnings (logged via the `log` crate).
/// Replacements inherit the mutation's seq number for future addressing.
///
/// Complexity: O(N*M) in worst case because each mutation may scan current
/// effective entries with linear `position`/`rposition` lookups. Acceptable for
/// typical session sizes.
pub fn apply_log_mutations(
    raw_entries: &[(usize, SessionLogEntry)],
) -> Vec<(usize, SessionLogEntry)> {
    apply_log_mutations_with_name(raw_entries, "session")
}

/// NATS-seq wrapper for [`apply_log_mutations`].
///
/// Preserves exact mutation semantics while accepting JetStream `u64`
/// sequence numbers and returning same typed sequence numbers.
pub fn apply_log_mutations_nats(
    raw_entries: &[(u64, SessionLogEntry)],
) -> Result<Vec<(u64, SessionLogEntry)>> {
    let raw_with_seq: Vec<_> = raw_entries
        .iter()
        .map(|(seq, entry)| {
            let Ok(seq) = usize::try_from(*seq) else {
                bail!("JetStream seq {seq} does not fit into usize");
            };
            Ok((seq, entry.clone()))
        })
        .collect::<Result<_>>()?;
    Ok(apply_log_mutations(&raw_with_seq)
        .into_iter()
        .map(|(seq, entry)| {
            // SAFETY: effective seqs are a subset of input seqs. Resolver only
            // deletes/replaces existing entries, and replacements inherit the
            // EditEntries seq, so no new seq can exceed input JetStream seq range.
            (u64::try_from(seq).expect("effective seq fits u64"), entry)
        })
        .collect())
}

/// Variant that accepts a session name for logging context.
pub fn apply_log_mutations_with_name(
    raw_entries: &[(usize, SessionLogEntry)],
    name: &str,
) -> Vec<(usize, SessionLogEntry)> {
    let mut effective_entries = Vec::new();

    for (seq, entry) in raw_entries {
        match entry {
            SessionLogEntry::Rewind { after_seq } => {
                // Rewind can only reference entries that come before it in the log
                if !effective_entries
                    .iter()
                    .any(|(existing_seq, _)| existing_seq == after_seq)
                {
                    log::warn!(
                        "Skipping rewind entry #{seq} in session {name}: after_seq {after_seq} not present in replay state"
                    );
                    continue;
                }
                effective_entries.retain(|(existing_seq, _)| *existing_seq <= *after_seq);
            }
            SessionLogEntry::EditEntries {
                from,
                to,
                replacements,
            } => {
                if from > to {
                    log::warn!(
                        "Skipping edit_entries entry #{seq} in session {name}: invalid range [{from}, {to}]"
                    );
                    continue;
                }
                // Note: to >= seq check is SKIPPED intentionally for NATS sequences
                // which start at 1 (not 0). The validation that from/to reference
                // earlier entries is done by checking they exist in effective_entries.

                let Some(start_idx) = effective_entries
                    .iter()
                    .position(|(existing_seq, _)| existing_seq == from)
                else {
                    log::warn!(
                        "Skipping edit_entries entry #{seq} in session {name}: from seq {from} not present in replay state"
                    );
                    continue;
                };
                let Some(end_idx) = effective_entries
                    .iter()
                    .rposition(|(existing_seq, _)| existing_seq == to)
                else {
                    log::warn!(
                        "Skipping edit_entries entry #{seq} in session {name}: to seq {to} not present in replay state"
                    );
                    continue;
                };
                if start_idx > end_idx {
                    log::warn!(
                        "Skipping edit_entries entry #{seq} in session {name}: range [{from}, {to}] not in replay order"
                    );
                    continue;
                }

                let parsed_replacements: Vec<_> = replacements
                    .iter()
                    .enumerate()
                    .filter_map(|(replacement_idx, replacement)| {
                        match serde_yaml::from_str::<SessionLogEntry>(replacement) {
                            Ok(parsed) => {
                                // Replacements inherit EditEntries seq because originals are
                                // logically removed from effective stream. Future rewind/edit
                                // operations must target mutation seq, not replaced seqs.
                                Some((*seq, parsed))
                            }
                            Err(err) => {
                                log::warn!(
                                    "Skipping replacement #{replacement_idx} in edit_entries entry #{seq} for session {name}: {err}"
                                );
                                None
                            }
                        }
                    })
                    .collect();

                effective_entries.splice(start_idx..=end_idx, parsed_replacements);
            }
            // TurnEnd is durable control metadata, not a logical transcript
            // row. Drop it from effective history so it cannot shift the
            // user-visible logical sequence numbers.
            SessionLogEntry::TurnEnd { .. } | SessionLogEntry::Unknown => {}
            _ => effective_entries.push((*seq, entry.clone())),
        }
    }

    effective_entries
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogicalEntryRef<P> {
    pub logical_index: usize,
    pub physical_seq: P,
}

pub trait PhysicalSessionEntry {
    type PhysicalSeq: Copy;

    fn physical_seq(&self) -> Self::PhysicalSeq;
    fn entry(&self) -> &SessionLogEntry;
}

impl PhysicalSessionEntry for (usize, SessionLogEntry) {
    type PhysicalSeq = usize;

    fn physical_seq(&self) -> Self::PhysicalSeq {
        self.0
    }

    fn entry(&self) -> &SessionLogEntry {
        &self.1
    }
}

impl PhysicalSessionEntry for (u64, SessionLogEntry) {
    type PhysicalSeq = u64;

    fn physical_seq(&self) -> Self::PhysicalSeq {
        self.0
    }

    fn entry(&self) -> &SessionLogEntry {
        &self.1
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveContextWindow<'a, T: PhysicalSessionEntry> {
    entries: &'a [T],
    boundary_index: Option<usize>,
}

impl<'a, T: PhysicalSessionEntry> ActiveContextWindow<'a, T> {
    pub fn entries(&self) -> &'a [T] {
        self.entries
    }

    pub fn boundary_index(&self) -> Option<usize> {
        self.boundary_index
    }

    pub fn logical_entries(&self) -> impl Iterator<Item = LogicalEntryRef<T::PhysicalSeq>> + '_ {
        self.entries
            .iter()
            .enumerate()
            .map(|(logical_index, entry)| LogicalEntryRef {
                logical_index,
                physical_seq: entry.physical_seq(),
            })
    }

    pub fn physical_seq_for_logical(&self, logical_index: usize) -> Option<T::PhysicalSeq> {
        self.entries
            .get(logical_index)
            .map(PhysicalSessionEntry::physical_seq)
    }

    pub fn logical_indices_for_physical(
        &self,
        physical_seq: T::PhysicalSeq,
    ) -> impl Iterator<Item = usize> + '_
    where
        T::PhysicalSeq: PartialEq,
    {
        self.logical_entries()
            .filter(move |entry| entry.physical_seq == physical_seq)
            .map(|entry| entry.logical_index)
    }

    pub fn is_protected_logical_index(&self, logical_index: usize) -> bool {
        self.entries
            .get(logical_index)
            .map(PhysicalSessionEntry::entry)
            .is_some_and(is_context_boundary)
    }
}

pub fn active_context_window<T: PhysicalSessionEntry>(entries: &[T]) -> ActiveContextWindow<'_, T> {
    let boundary_index = entries
        .iter()
        .rposition(|entry| is_context_boundary(entry.entry()));
    match boundary_index {
        Some(index) if matches!(entries[index].entry(), SessionLogEntry::Header { .. }) => {
            ActiveContextWindow {
                entries: &entries[index..],
                boundary_index: Some(index),
            }
        }
        Some(index) => ActiveContextWindow {
            entries: &entries[index + 1..],
            boundary_index: Some(index),
        },
        None => ActiveContextWindow {
            entries,
            boundary_index: None,
        },
    }
}

fn is_context_boundary(entry: &SessionLogEntry) -> bool {
    matches!(
        entry,
        SessionLogEntry::Header { .. } | SessionLogEntry::Compress { .. }
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnStatus {
    Idle,
    InFlightResumable,
    InFlightCancelled,
}

#[derive(Debug, Clone)]
pub struct ReconstructedState {
    pub turn_status: TurnStatus,
    pub next_turn_messages: Vec<Message>,
    pub resumable_ctx: Option<ResumableCtx>,
}

#[derive(Debug, Clone)]
pub struct ResumableCtx {
    pub last_user: Option<Message>,
    pub last_assistant: Option<AssistantTurn>,
    pub pending_tool_results: Vec<ToolOutput>,
    pub fence_token: Option<u64>,
}

#[derive(Debug, Clone)]
pub enum AssistantTurn {
    Message(Message),
    ToolCalls {
        text: String,
        thought: Option<String>,
        calls: Vec<ToolCall>,
    },
}

/// Reconstruct session state from raw log entries.
///
/// This function applies mutation entries (EditEntries, Rewind) automatically
/// before reconstruction, so retract/edit operations are honored.
///
/// For direct control over mutation resolution, use `apply_log_mutations` then
/// `reconstruct_state_effective`.
pub fn reconstruct_state(entries: &[SessionLogEntry]) -> ReconstructedState {
    // Apply mutations first: convert raw entries to effective entries
    // Use 0-based indexing for local entries
    let raw_with_seq: Vec<_> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| (i, e.clone()))
        .collect();
    let effective_entries = apply_log_mutations(&raw_with_seq);

    // Convert to the format expected by reconstruct_state_with_seq
    reconstruct_state_effective(&effective_entries)
}

/// Reconstruct session state from entries with NATS JetStream sequence numbers.
///
/// This function applies mutation entries (EditEntries, Rewind) automatically
/// before reconstruction, so retract/edit operations are honored.
///
/// NATS sequences start at 1, not 0.
pub fn reconstruct_state_from_nats(entries: &[(u64, SessionLogEntry)]) -> ReconstructedState {
    // Convert u64 sequences to usize once upfront, avoiding the double round-trip
    // through apply_log_mutations_nats (which would convert u64→usize→u64, then we'd
    // convert back to usize for reconstruct_state_effective).
    let raw_with_usize: Vec<_> = match entries
        .iter()
        .map(|(seq, entry)| {
            usize::try_from(*seq)
                .map(|s| (s, entry.clone()))
                .map_err(|_| format!("JetStream seq {seq} does not fit into usize"))
        })
        .collect::<Result<_, _>>()
    {
        Ok(v) => v,
        Err(err) => {
            log::warn!("failed to convert NATS sequences during reconstruction: {err}");
            return reconstruct_state_effective(&[]);
        }
    };
    let effective_entries = apply_log_mutations(&raw_with_usize);
    reconstruct_state_effective(&effective_entries)
}

/// Reconstruct from pre-resolved effective entries (mutations already applied).
pub fn reconstruct_state_effective(entries: &[(usize, SessionLogEntry)]) -> ReconstructedState {
    let with_seq: Vec<_> = entries
        .iter()
        .map(|(seq, e)| (Some(*seq), e.clone()))
        .collect();
    reconstruct_state_with_seq(&with_seq)
}

/// Variant that accepts entries with JetStream sequence numbers attached.
///
/// IMPORTANT: Callers should use `reconstruct_state` or ensure they pass
/// already-resolved entries. This function does NOT apply mutations.
pub fn reconstruct_state_with_seq(
    entries: &[(Option<usize>, SessionLogEntry)],
) -> ReconstructedState {
    let tail_indices = entries_after_last_barrier_with_seq(entries);
    let mut replay = ReplayAccumulator::default();

    for (idx, entry) in tail_indices {
        replay.apply_entry_with_seq(idx, entry);
    }

    replay.finish()
}

#[derive(Debug, Default)]
struct ReplayAccumulator {
    next_turn_messages: Vec<Message>,
    resumable_last_user: Option<Message>,
    resumable_last_assistant: Option<AssistantTurn>,
    pending_tool_results: Vec<ToolOutput>,
    active_fence_token: Option<u64>,
    cancel_fence_token: Option<u64>,
}

impl ReplayAccumulator {
    #[allow(dead_code)]
    fn apply_entry(&mut self, entry: &SessionLogEntry) {
        self.apply_entry_with_seq(None, entry);
    }

    fn apply_entry_with_seq(&mut self, log_seq: Option<usize>, entry: &SessionLogEntry) {
        match entry {
            SessionLogEntry::Cancel { fence_token } => self.on_cancel(*fence_token),
            SessionLogEntry::Message { role, .. } if role.is_user() => {
                self.on_user_message_with_seq(log_seq, entry)
            }
            SessionLogEntry::Message { role, .. } if role.is_assistant() => {
                self.on_assistant_message(entry)
            }
            SessionLogEntry::ToolCalls { .. } => self.on_tool_calls(entry),
            SessionLogEntry::ToolResults { results, .. } => self.on_tool_results(results),
            _ => {}
        }
    }

    fn finish(self) -> ReconstructedState {
        let is_resumable = matches!(
            self.resumable_last_assistant,
            Some(AssistantTurn::ToolCalls { .. })
        );

        if let Some(fence_token) = self.cancel_fence_token {
            return ReconstructedState {
                turn_status: TurnStatus::InFlightCancelled,
                next_turn_messages: Vec::new(),
                resumable_ctx: Some(ResumableCtx {
                    last_user: None,
                    last_assistant: None,
                    pending_tool_results: Vec::new(),
                    fence_token: Some(fence_token),
                }),
            };
        }

        if !self.next_turn_messages.is_empty() && !is_resumable {
            return ReconstructedState {
                turn_status: TurnStatus::Idle,
                next_turn_messages: self.next_turn_messages,
                resumable_ctx: None,
            };
        }

        let next_turn_messages = self.next_turn_messages;
        ReconstructedState {
            turn_status: if is_resumable {
                TurnStatus::InFlightResumable
            } else {
                TurnStatus::Idle
            },
            next_turn_messages: if is_resumable {
                next_turn_messages
            } else {
                Vec::new()
            },
            resumable_ctx: is_resumable.then_some(ResumableCtx {
                last_user: self.resumable_last_user,
                last_assistant: self.resumable_last_assistant,
                pending_tool_results: self.pending_tool_results,
                fence_token: self.active_fence_token,
            }),
        }
    }

    fn on_cancel(&mut self, fence_token: u64) {
        self.cancel_fence_token = Some(fence_token);
        self.resumable_last_user = None;
        self.resumable_last_assistant = None;
        self.pending_tool_results.clear();
        self.active_fence_token = None;
    }

    #[allow(dead_code)]
    fn on_user_message(&mut self, entry: &SessionLogEntry) {
        self.on_user_message_with_seq(None, entry);
    }

    fn on_user_message_with_seq(&mut self, log_seq: Option<usize>, entry: &SessionLogEntry) {
        let Some(mut message) = cloned_message(entry) else {
            return;
        };
        // Attach log_seq if provided
        if let Some(seq) = log_seq {
            message.log_seq = Some(seq);
        }

        if self.cancel_fence_token.is_some() {
            self.cancel_fence_token = None;
            self.next_turn_messages.clear();
        }

        if matches!(
            self.resumable_last_assistant,
            Some(AssistantTurn::ToolCalls { .. })
        ) {
            self.next_turn_messages.push(message);
            return;
        }

        self.next_turn_messages.push(message.clone());
        self.resumable_last_user = None;
        self.resumable_last_assistant = None;
        self.pending_tool_results.clear();
        self.active_fence_token = None;
    }

    fn on_assistant_message(&mut self, _entry: &SessionLogEntry) {
        self.next_turn_messages.clear();
        self.resumable_last_user = None;
        self.resumable_last_assistant = None;
        self.pending_tool_results.clear();
        self.active_fence_token = None;
        self.cancel_fence_token = None;
    }

    fn on_tool_calls(&mut self, entry: &SessionLogEntry) {
        if self.cancel_fence_token.is_some() {
            return;
        }

        let SessionLogEntry::ToolCalls {
            text,
            thought,
            calls,
            fence_token,
            ..
        } = entry
        else {
            return;
        };

        self.resumable_last_user = self.next_turn_messages.last().cloned();
        self.next_turn_messages.clear();
        self.resumable_last_assistant = Some(AssistantTurn::ToolCalls {
            text: text.clone(),
            thought: thought.clone(),
            calls: calls.clone(),
        });
        self.pending_tool_results.clear();
        self.active_fence_token = *fence_token;
    }

    fn on_tool_results(&mut self, results: &[ToolOutput]) {
        if self.cancel_fence_token.is_none()
            && matches!(
                self.resumable_last_assistant,
                Some(AssistantTurn::ToolCalls { .. })
            )
        {
            self.pending_tool_results = results.to_vec();
        }
    }
}

fn cloned_message(entry: &SessionLogEntry) -> Option<Message> {
    let SessionLogEntry::Message {
        id,
        role,
        content,
        timestamp,
        ..
    } = entry
    else {
        return None;
    };

    Some(Message {
        role: *role,
        content: content.clone(),
        id: id.clone(),
        log_seq: None,
        log_timestamp: *timestamp,
    })
}

#[allow(dead_code)]
fn entries_after_last_barrier(entries: &[SessionLogEntry]) -> &[SessionLogEntry] {
    let last_barrier = entries
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, entry)| is_final_assistant_message(entry).then_some(index));

    last_barrier.map_or(entries, |index| &entries[index + 1..])
}

fn entries_after_last_barrier_with_seq(
    entries: &[(Option<usize>, SessionLogEntry)],
) -> Vec<(Option<usize>, &SessionLogEntry)> {
    let last_barrier = entries
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, (_, entry))| is_final_assistant_message(entry).then_some(index));

    last_barrier.map_or_else(
        || entries.iter().map(|(seq, e)| (*seq, e)).collect(),
        |index| {
            entries[index + 1..]
                .iter()
                .map(|(seq, e)| (*seq, e))
                .collect()
        },
    )
}

fn is_final_assistant_message(entry: &SessionLogEntry) -> bool {
    // A barrier is any final assistant text Message. Tool calls live in separate
    // `ToolCalls` entries, so an assistant `Message` always marks the end of a
    // completed turn. The fence_token is `Some(..)` for worker-originated (NATS HA)
    // turns and `None` for local-mode turns — BOTH are barriers. The client-side
    // `id` is irrelevant to barrier detection.
    matches!(
        entry,
        SessionLogEntry::Message {
            role: MessageRole::Assistant,
            ..
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::MessageContent;
    use serde_json::json;

    #[test]
    fn empty_log_is_idle_with_no_next_turn_messages() {
        assert_case(
            vec![],
            ExpectedState::Idle {
                next_turn_messages: &[],
            },
        );
    }

    #[test]
    fn barrier_only_is_idle_with_no_next_turn_messages() {
        assert_case(
            vec![final_assistant("done")],
            ExpectedState::Idle {
                next_turn_messages: &[],
            },
        );
    }

    #[test]
    fn post_barrier_single_user_folds_into_next_turn_messages() {
        assert_case(
            vec![final_assistant("done"), user_message("u1")],
            ExpectedState::Idle {
                next_turn_messages: &["u1"],
            },
        );
    }

    #[test]
    fn post_barrier_multiple_users_fold_in_order() {
        assert_case(
            vec![
                final_assistant("done"),
                user_message("u1"),
                user_message("u2"),
                user_message("u3"),
            ],
            ExpectedState::Idle {
                next_turn_messages: &["u1", "u2", "u3"],
            },
        );
    }

    #[test]
    fn tool_calls_after_post_barrier_user_are_resumable() {
        assert_case(
            vec![
                final_assistant("done"),
                user_message("u1"),
                tool_calls_assistant(7),
            ],
            ExpectedState::Resumable {
                fence_token: Some(7),
                last_user: Some("u1"),
                assistant: ExpectedAssistant::ToolCalls,
                pending_tool_results_len: 0,
                next_turn_messages: &[],
            },
        );
    }

    #[test]
    fn users_after_tool_calls_queue_for_next_turn() {
        assert_case(
            vec![
                final_assistant("done"),
                user_message("u1"),
                tool_calls_assistant(7),
                user_message("u2"),
            ],
            ExpectedState::Resumable {
                fence_token: Some(7),
                last_user: Some("u1"),
                assistant: ExpectedAssistant::ToolCalls,
                pending_tool_results_len: 0,
                next_turn_messages: &["u2"],
            },
        );
    }

    #[test]
    fn tool_results_without_matching_tool_calls_are_ignored() {
        assert_case(
            vec![final_assistant("done"), tool_results("tool-a")],
            ExpectedState::Idle {
                next_turn_messages: &[],
            },
        );
    }

    #[test]
    fn tool_results_attach_to_resumable_ctx_but_keep_queued_users() {
        assert_case(
            vec![
                final_assistant("done"),
                user_message("u1"),
                tool_calls_assistant(7),
                user_message("u2"),
                tool_results("tool-a"),
            ],
            ExpectedState::Resumable {
                fence_token: Some(7),
                last_user: Some("u1"),
                assistant: ExpectedAssistant::ToolCalls,
                pending_tool_results_len: 1,
                next_turn_messages: &["u2"],
            },
        );
    }

    #[test]
    fn cancel_after_tool_calls_marks_inflight_cancelled() {
        assert_case(
            vec![
                final_assistant("done"),
                user_message("u1"),
                tool_calls_assistant(7),
                cancel(7),
            ],
            ExpectedState::Cancelled {
                fence_token: Some(7),
            },
        );
    }

    #[test]
    fn user_after_cancel_starts_next_turn_and_clears_tombstone() {
        assert_case(
            vec![
                final_assistant("done"),
                user_message("u1"),
                tool_calls_assistant(7),
                cancel(7),
                user_message("u2"),
            ],
            ExpectedState::Idle {
                next_turn_messages: &["u2"],
            },
        );
    }

    #[test]
    fn latest_cancel_tombstone_wins() {
        assert_case(
            vec![
                final_assistant("done"),
                user_message("u1"),
                tool_calls_assistant(7),
                cancel(7),
                cancel(8),
            ],
            ExpectedState::Cancelled {
                fence_token: Some(8),
            },
        );
    }

    #[test]
    fn cancel_without_barrier_is_cancelled() {
        assert_case(
            vec![cancel(9)],
            ExpectedState::Cancelled {
                fence_token: Some(9),
            },
        );
    }

    #[test]
    fn tool_results_without_tool_calls_after_barrier_stay_idle() {
        assert_case(
            vec![final_assistant("done"), tool_results("tool-a")],
            ExpectedState::Idle {
                next_turn_messages: &[],
            },
        );
    }

    #[test]
    fn final_assistant_message_is_barrier_even_after_cancelled_tail() {
        assert_case(
            vec![cancel(3), final_assistant("done")],
            ExpectedState::Idle {
                next_turn_messages: &[],
            },
        );
    }

    #[test]
    fn fenced_final_assistant_is_a_barrier() {
        // Worker-originated (NATS HA) final assistant messages carry a fence_token.
        // They MUST still be recognized as turn barriers, otherwise the worker
        // re-folds already-answered user messages and re-runs completed turns.
        assert_case(
            vec![user_message("u1"), fenced_final_assistant("done", 5)],
            ExpectedState::Idle {
                next_turn_messages: &[],
            },
        );
    }

    fn to_yaml(entry: &SessionLogEntry) -> String {
        serde_yaml::to_string(entry).expect("serialize replacement")
    }

    fn edited_user_replacement_yaml() -> String {
        serde_yaml::to_string(&SessionLogEntry::Message {
            id: Some("edited-id".to_string()),
            role: MessageRole::User,
            content: MessageContent::Text("edited".to_string()),
            timestamp: None,
            fence_token: None,
        })
        .expect("serialize replacement")
    }

    fn apply_edit_replacement_case(
        from: usize,
        to: usize,
        replacements: Vec<String>,
        prefix_entries: Vec<(usize, SessionLogEntry)>,
        edit_seq: usize,
        suffix_entries: Vec<(usize, SessionLogEntry)>,
    ) -> Vec<(usize, SessionLogEntry)> {
        let mut entries = prefix_entries;
        entries.push((
            edit_seq,
            SessionLogEntry::EditEntries {
                from,
                to,
                replacements,
            },
        ));
        entries.extend(suffix_entries);
        apply_log_mutations(&entries)
    }

    #[test]
    fn edit_entries_replacement_yaml_is_applied() {
        let effective = apply_edit_replacement_case(
            1,
            1,
            vec![edited_user_replacement_yaml()],
            vec![(1, user_message("original"))],
            2,
            vec![],
        );

        assert_eq!(effective.len(), 1);
        assert_eq!(effective[0].0, 2);
        assert_eq!(message_text(&entry_as_message(&effective[0].1)), "edited");
    }

    /// Apply `[first, second, edit]` and assert the malformed `edit` was skipped,
    /// leaving the original two user messages untouched.
    fn assert_edit_is_skipped(edit: SessionLogEntry) {
        let effective = apply_log_mutations(&[
            (1, user_message("first")),
            (2, user_message("second")),
            (3, edit),
        ]);

        assert_eq!(effective.len(), 2);
        assert_eq!(effective[0].0, 1);
        assert_eq!(effective[1].0, 2);
        assert_eq!(message_text(&entry_as_message(&effective[0].1)), "first");
        assert_eq!(message_text(&entry_as_message(&effective[1].1)), "second");
    }

    #[test]
    fn edit_entries_invalid_range_is_skipped() {
        assert_edit_is_skipped(SessionLogEntry::EditEntries {
            from: 2,
            to: 1,
            replacements: vec![edited_user_replacement_yaml()],
        });
    }

    #[test]
    fn edit_entries_missing_from_seq_is_skipped() {
        assert_edit_is_skipped(SessionLogEntry::EditEntries {
            from: 99,
            to: 2,
            replacements: vec![edited_user_replacement_yaml()],
        });
    }

    #[test]
    fn edit_entries_missing_to_seq_is_skipped() {
        assert_edit_is_skipped(SessionLogEntry::EditEntries {
            from: 1,
            to: 99,
            replacements: vec![edited_user_replacement_yaml()],
        });
    }

    #[test]
    fn edit_entries_not_in_replay_order_is_skipped() {
        let effective = apply_log_mutations(&[
            (1, user_message("first")),
            (2, user_message("second")),
            (
                3,
                SessionLogEntry::EditEntries {
                    from: 1,
                    to: 1,
                    replacements: vec![to_yaml(&user_message("first replacement one"))],
                },
            ),
            (
                4,
                SessionLogEntry::EditEntries {
                    from: 2,
                    to: 3,
                    replacements: vec![edited_user_replacement_yaml()],
                },
            ),
        ]);

        assert_eq!(effective.len(), 2);
        assert_eq!(effective[0].0, 3);
        assert_eq!(effective[1].0, 2);
        assert_eq!(
            message_text(&entry_as_message(&effective[0].1)),
            "first replacement one"
        );
        assert_eq!(message_text(&entry_as_message(&effective[1].1)), "second");
    }

    #[test]
    fn rewind_missing_after_seq_is_skipped() {
        let effective = apply_log_mutations(&[
            (1, user_message("first")),
            (2, final_assistant("done")),
            (3, SessionLogEntry::Rewind { after_seq: 99 }),
        ]);

        assert_eq!(effective.len(), 2);
        assert_eq!(effective[0].0, 1);
        assert_eq!(effective[1].0, 2);
        assert_eq!(message_text(&entry_as_message(&effective[0].1)), "first");
        assert_eq!(message_text(&entry_as_message(&effective[1].1)), "done");
    }

    #[test]
    fn edit_entries_all_malformed_replacements_delete_target_range() {
        let effective = apply_log_mutations(&[
            (1, user_message("first")),
            (2, user_message("second")),
            (
                3,
                SessionLogEntry::EditEntries {
                    from: 1,
                    to: 2,
                    replacements: vec![": not valid yaml for session entry".to_string()],
                },
            ),
            (4, user_message("tail")),
        ]);

        assert_eq!(effective.len(), 1);
        assert_eq!(effective[0].0, 4);
        assert_eq!(message_text(&entry_as_message(&effective[0].1)), "tail");
    }

    #[test]
    fn edit_entries_malformed_replacement_is_dropped_while_valid_one_is_applied() {
        let effective = apply_edit_replacement_case(
            1,
            2,
            vec![
                edited_user_replacement_yaml(),
                ": not valid yaml for session entry".to_string(),
            ],
            vec![(1, user_message("original")), (2, final_assistant("done"))],
            3,
            vec![],
        );

        assert_eq!(effective.len(), 1);
        assert_eq!(effective[0].0, 3);
        assert_eq!(message_text(&entry_as_message(&effective[0].1)), "edited");
    }

    #[test]
    fn rewind_truncates_effective_entries_after_prior_edit() {
        let effective = apply_edit_replacement_case(
            1,
            1,
            vec![edited_user_replacement_yaml()],
            vec![(1, user_message("original"))],
            2,
            vec![
                (3, final_assistant("done")),
                (4, SessionLogEntry::Rewind { after_seq: 2 }),
            ],
        );

        assert_eq!(effective.len(), 1);
        assert_eq!(effective[0].0, 2);
        assert_eq!(message_text(&entry_as_message(&effective[0].1)), "edited");
    }

    #[test]
    fn post_fenced_barrier_user_folds_into_next_turn() {
        assert_case(
            vec![
                user_message("u1"),
                fenced_final_assistant("answer1", 5),
                user_message("u2"),
            ],
            ExpectedState::Idle {
                next_turn_messages: &["u2"],
            },
        );
    }

    enum ExpectedState<'a> {
        Idle {
            next_turn_messages: &'a [&'a str],
        },
        Resumable {
            fence_token: Option<u64>,
            last_user: Option<&'a str>,
            assistant: ExpectedAssistant,
            pending_tool_results_len: usize,
            next_turn_messages: &'a [&'a str],
        },
        Cancelled {
            fence_token: Option<u64>,
        },
    }

    enum ExpectedAssistant {
        ToolCalls,
    }

    fn assert_case(entries: Vec<SessionLogEntry>, expected: ExpectedState<'_>) {
        let state = reconstruct_state(&entries);
        match expected {
            ExpectedState::Idle { next_turn_messages } => {
                assert_eq!(state.turn_status, TurnStatus::Idle);
                assert_next_turn_messages(&state.next_turn_messages, next_turn_messages);
                assert!(state.resumable_ctx.is_none());
            }
            ExpectedState::Resumable {
                fence_token,
                last_user,
                assistant,
                pending_tool_results_len,
                next_turn_messages,
            } => {
                assert_eq!(state.turn_status, TurnStatus::InFlightResumable);
                assert_next_turn_messages(&state.next_turn_messages, next_turn_messages);
                let ctx = state.resumable_ctx.expect("resumable ctx");
                assert_eq!(ctx.fence_token, fence_token);
                assert_eq!(
                    ctx.last_user.as_ref().map(message_text).as_deref(),
                    last_user
                );
                match assistant {
                    ExpectedAssistant::ToolCalls => {
                        assert!(matches!(
                            ctx.last_assistant,
                            Some(AssistantTurn::ToolCalls { .. })
                        ));
                    }
                }
                assert_eq!(ctx.pending_tool_results.len(), pending_tool_results_len);
            }
            ExpectedState::Cancelled { fence_token } => {
                assert_eq!(state.turn_status, TurnStatus::InFlightCancelled);
                assert!(state.next_turn_messages.is_empty());
                let ctx = state.resumable_ctx.expect("cancel ctx");
                assert_eq!(ctx.fence_token, fence_token);
                assert!(ctx.last_user.is_none());
                assert!(ctx.last_assistant.is_none());
                assert!(ctx.pending_tool_results.is_empty());
            }
        }
    }

    fn assert_next_turn_messages(messages: &[Message], expected: &[&str]) {
        let actual: Vec<_> = messages.iter().map(message_text).collect();
        assert_eq!(actual, expected);
    }

    fn message_text(message: &Message) -> String {
        message.content.to_text()
    }

    fn final_assistant(text: &str) -> SessionLogEntry {
        SessionLogEntry::Message {
            id: None,
            role: MessageRole::Assistant,
            content: MessageContent::Text(text.to_string()),
            timestamp: None,
            fence_token: None,
        }
    }

    fn entry_as_message(entry: &SessionLogEntry) -> Message {
        match entry {
            SessionLogEntry::Message {
                id,
                role,
                content,
                timestamp,
                ..
            } => Message {
                role: *role,
                content: content.clone(),
                id: id.clone(),
                log_seq: None,
                log_timestamp: *timestamp,
            },
            other => panic!("expected message entry, got {other:?}"),
        }
    }

    fn fenced_final_assistant(text: &str, fence_token: u64) -> SessionLogEntry {
        SessionLogEntry::Message {
            id: None,
            role: MessageRole::Assistant,
            content: MessageContent::Text(text.to_string()),
            timestamp: None,
            fence_token: Some(fence_token),
        }
    }

    fn user_message(text: &str) -> SessionLogEntry {
        SessionLogEntry::Message {
            id: None,
            role: MessageRole::User,
            content: MessageContent::Text(text.to_string()),
            timestamp: None,
            fence_token: None,
        }
    }

    fn tool_calls_assistant(fence_token: u64) -> SessionLogEntry {
        SessionLogEntry::ToolCalls {
            text: "working".to_string(),
            thought: Some("thinking".to_string()),
            calls: vec![ToolCall::new(
                "tool-a".to_string(),
                json!({"arg": 1}),
                Some("call-1".to_string()),
                None,
            )],
            timestamp: None,
            fence_token: Some(fence_token),
        }
    }

    fn tool_results(name: &str) -> SessionLogEntry {
        SessionLogEntry::ToolResults {
            results: vec![ToolOutput {
                id: Some("call-1".to_string()),
                name: name.to_string(),
                output: json!({"ok": true}),
                markdown: None,
                content: vec![],
                switch_agent: None,
            }],
            timestamp: None,
        }
    }

    #[test]
    fn active_context_window_includes_header_boundary_as_logical_zero() {
        let entries = vec![
            (10_u64, header_entry()),
            (11, user_message("u1")),
            (12, final_assistant("a1")),
        ];

        let window = active_context_window(&entries);
        let logical_entries: Vec<_> = window.logical_entries().collect();

        assert_eq!(window.boundary_index(), Some(0));
        assert_eq!(window.entries().len(), 3);
        assert_eq!(
            logical_entries,
            vec![
                LogicalEntryRef {
                    logical_index: 0,
                    physical_seq: 10
                },
                LogicalEntryRef {
                    logical_index: 1,
                    physical_seq: 11
                },
                LogicalEntryRef {
                    logical_index: 2,
                    physical_seq: 12
                },
            ]
        );
        assert_eq!(window.physical_seq_for_logical(0), Some(10));
        assert_eq!(window.physical_seq_for_logical(1), Some(11));
        assert!(window.is_protected_logical_index(0));
        assert!(!window.is_protected_logical_index(1));
        assert_eq!(
            window.logical_indices_for_physical(10).collect::<Vec<_>>(),
            vec![0]
        );
    }

    #[test]
    fn active_context_window_uses_whole_log_when_headerless() {
        let entries = vec![(0_usize, user_message("u1")), (1, final_assistant("a1"))];

        let window = active_context_window(&entries);
        let logical_entries: Vec<_> = window.logical_entries().collect();

        assert_eq!(window.boundary_index(), None);
        assert_eq!(window.entries().len(), 2);
        assert_eq!(
            logical_entries,
            vec![
                LogicalEntryRef {
                    logical_index: 0,
                    physical_seq: 0
                },
                LogicalEntryRef {
                    logical_index: 1,
                    physical_seq: 1
                },
            ]
        );
        assert_eq!(window.physical_seq_for_logical(0), Some(0));
        assert_eq!(window.physical_seq_for_logical(1), Some(1));
        assert!(!window.is_protected_logical_index(0));
    }

    #[test]
    fn active_context_window_starts_after_most_recent_compress() {
        let entries = vec![
            (20_u64, header_entry()),
            (21, user_message("u1")),
            (22, final_assistant("a1")),
            (
                30,
                SessionLogEntry::Compress {
                    prompt: "summary".to_string(),
                },
            ),
            (31, user_message("u2")),
            (32, final_assistant("a2")),
        ];

        let window = active_context_window(&entries);
        let logical_entries: Vec<_> = window.logical_entries().collect();

        assert_eq!(window.boundary_index(), Some(3));
        assert_eq!(window.entries().len(), 2);
        assert_eq!(
            logical_entries,
            vec![
                LogicalEntryRef {
                    logical_index: 0,
                    physical_seq: 31
                },
                LogicalEntryRef {
                    logical_index: 1,
                    physical_seq: 32
                },
            ]
        );
        assert_eq!(window.physical_seq_for_logical(0), Some(31));
        assert_eq!(
            window.logical_indices_for_physical(30).collect::<Vec<_>>(),
            Vec::<usize>::new()
        );
    }

    #[test]
    fn active_context_window_preserves_many_logical_entries_for_one_physical_seq() {
        let entries = vec![
            (40_u64, header_entry()),
            (50, user_message("u1")),
            (
                51,
                SessionLogEntry::EditEntries {
                    from: 50,
                    to: 50,
                    replacements: vec![
                        to_yaml(&user_message("u1 clone")),
                        to_yaml(&user_message("u2 clone")),
                        to_yaml(&final_assistant("a1 clone")),
                    ],
                },
            ),
        ];
        let effective = apply_log_mutations_nats(&entries).expect("mutations apply");
        let window = active_context_window(&effective);
        let logical_entries: Vec<_> = window.logical_entries().collect();

        assert_eq!(window.boundary_index(), Some(0));
        assert_eq!(
            logical_entries,
            vec![
                LogicalEntryRef {
                    logical_index: 0,
                    physical_seq: 40
                },
                LogicalEntryRef {
                    logical_index: 1,
                    physical_seq: 51
                },
                LogicalEntryRef {
                    logical_index: 2,
                    physical_seq: 51
                },
                LogicalEntryRef {
                    logical_index: 3,
                    physical_seq: 51
                },
            ]
        );
        assert_eq!(window.physical_seq_for_logical(1), Some(51));
        assert_eq!(window.physical_seq_for_logical(2), Some(51));
        assert_eq!(window.physical_seq_for_logical(3), Some(51));
        assert_eq!(
            window.logical_indices_for_physical(51).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    fn header_entry() -> SessionLogEntry {
        SessionLogEntry::Header {
            model_id: "model".to_string(),
            temperature: None,
            top_p: None,
            use_tools: None,
            compress_threshold: None,
            agent_name: None,
            session_id: None,
            working_dir: None,
            git_branch: None,
            git_remote: None,
            terminal_session_id: None,
            agent_variables: Default::default(),
            agent_instructions: String::new(),
            model_fallbacks: vec![],
            compaction_agent: None,
        }
    }

    fn cancel(fence_token: u64) -> SessionLogEntry {
        SessionLogEntry::Cancel { fence_token }
    }
}
