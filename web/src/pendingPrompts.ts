export interface PendingPrompt {
  id: string;
  messageId: string | null;
  text: string;
  attachmentNames: string[];
}

export interface PendingPromptsState {
  prompts: PendingPrompt[];
}

export const EMPTY_PENDING_PROMPTS: PendingPromptsState = { prompts: [] };
let nextPendingPromptId = 0;

export function updateSessionPendingPrompts(
  sessions: Record<string, PendingPromptsState>,
  key: string,
  update: (current: PendingPromptsState) => PendingPromptsState,
): Record<string, PendingPromptsState> {
  const previous = sessions[key] ?? EMPTY_PENDING_PROMPTS;
  const next = update(previous);
  if (next === previous) return sessions;
  if (next.prompts.length === 0) {
    const remaining = { ...sessions };
    delete remaining[key];
    return remaining;
  }
  return { ...sessions, [key]: next };
}

export function createPendingPrompt(
  text: string,
  attachments: readonly { name?: string; file?: File }[],
  messageId: string | null,
): PendingPrompt {
  return {
    id: `pending-prompt-${nextPendingPromptId++}`,
    messageId,
    text,
    attachmentNames: attachments.map((attachment) => attachment.name ?? attachment.file?.name ?? 'Attachment'),
  };
}

// The prompt RPC returns the durable user-message ID before the subscriber
// hydrates that row. Text is not a safe key: another tab may submit the same
// words, and server-side normalization may change them. Older servers without
// this ID keep their accepted row visible until the view is reloaded.
export function reconcilePendingPrompts(
  state: PendingPromptsState,
  userMessageIds: readonly string[] | null,
): PendingPromptsState {
  if (userMessageIds === null) return state;
  const ids = new Set(userMessageIds);
  const prompts = state.prompts.filter((prompt) => prompt.messageId === null || !ids.has(prompt.messageId));
  return prompts.length === state.prompts.length ? state : { prompts };
}

export function pendingUserMessageIds(
  state: PendingPromptsState,
  messages: readonly { role: string; id: string }[],
): string[] | null {
  if (state.prompts.length === 0) return null;
  return messages.filter((message) => message.role === 'user').map((message) => message.id);
}
