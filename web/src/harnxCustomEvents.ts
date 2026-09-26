import type { UsageData } from './UsageContext';
import { setDocumentTitle } from './sessionTitle';
import type { ToolCallUpdatePatch } from './toolUpdates';
import { parseToolUpdateEvent } from './toolUpdates';

/**
 * Internal Harnx control and metadata events that drive navigation, sequencing,
 * or UI state, but must NOT be forwarded to the assistant-ui runtime (they are not chat content).
 *
 * `message_attachments` carries per-message attachment metadata beyond what the pinned
 * `ag-ui-core = "=0.1.0"` user-message schema (String content, no parts/attachment field)
 * can represent. The CUSTOM event side-channel mirrors the control-state hydration pattern.
 */
export const NAVIGATION_CONTROL_EVENTS = [
  'session_attach_boundary',
  'session_handoff',
  'message_attachments',
] as const;

export interface MessageAttachmentMeta {
  partIndex: number;
  cid: string;
  kind: 'image';
}

/** Compaction outcome from session_compacting_completed event. */
export type CompactionOutcome =
  | { status: 'compacted' }
  | { status: 'unchanged'; detail?: string };

export interface HarnxCustomEventCallbacks {
  onStatus: (text: string | null) => void;
  onRunFailed: (message: string) => void;
  onUsage: (usage: UsageData) => void;
  onToolSummary: (id: string, summary: string) => void;
  onAttachBoundary?: (seq: number) => void;
  onHandoff?: (agent: string, sessionId: string, afterSeq: number) => void;
  /** Called when a hitl_pending_approval CUSTOM event is received. */
  onHitlPendingApproval?: (toolCallId: string, summary: string) => void;
  /** Called when a message_attachments CUSTOM event is received. */
  onMessageAttachments?: (messageId: string, attachments: MessageAttachmentMeta[]) => void;
  /** Called when session_compacting_started is received. */
  onCompactingStarted?: (compactionId?: string) => void;
  /** Called when session_compacting_completed is received. */
  onCompactingCompleted?: (outcome: CompactionOutcome, compactionId?: string) => void;
  /** Called when session_compacting_failed is received. */
  onCompactingFailed?: (error: string, compactionId?: string) => void;
  /** Called when a tool_update CUSTOM event is received. */
  onToolUpdate?: (patch: ToolCallUpdatePatch) => void;
  /**
   * Whether events belong to the session currently shown in the foreground.
   * When `false`, the `session_title_updated` handler skips `setDocumentTitle`
   * so child-session observers do not overwrite the browser tab title.
   */
  isForeground?: boolean;
}

type CustomEventHandler = (callbacks: HarnxCustomEventCallbacks, value: unknown) => void;

function eventRecord(value: unknown): Record<string, unknown> {
  return value !== null && typeof value === 'object' ? (value as Record<string, unknown>) : {};
}

function nonBlankString(value: unknown): string | undefined {
  return typeof value === 'string' && value.trim().length > 0 ? value : undefined;
}

function stringField(value: unknown, field: string): string | undefined {
  const candidate = eventRecord(value)[field];
  return typeof candidate === 'string' ? candidate : undefined;
}

function optionalNumber(value: unknown): boolean {
  return value === undefined || typeof value === 'number';
}

function optionalString(value: unknown): boolean {
  return value === undefined || typeof value === 'string';
}

function optionalNullableNumber(value: unknown): boolean {
  return value === undefined || value === null || typeof value === 'number';
}

function isUsageData(value: unknown): value is UsageData {
  const usage = eventRecord(value);
  const optionalNumbers = [usage.cached, usage.context_tokens, usage.context_percent];
  return (
    typeof usage.input === 'number' &&
    typeof usage.output === 'number' &&
    optionalNumbers.every(optionalNumber) &&
    optionalString(usage.session_label) &&
    optionalNullableNumber(usage.max_context_tokens)
  );
}

function validSeq(value: unknown): value is number {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= 0;
}

function isMessageAttachmentMeta(entry: unknown): entry is MessageAttachmentMeta {
  if (entry === null || typeof entry !== 'object') return false;
  const item = entry as Record<string, unknown>;
  return (
    typeof item.partIndex === 'number' &&
    Number.isSafeInteger(item.partIndex) &&
    item.partIndex >= 0 &&
    typeof item.cid === 'string' &&
    item.cid.trim().length > 0 &&
    item.kind === 'image'
  );
}

function parseMessageAttachments(value: unknown): {
  messageId: string;
  attachments: MessageAttachmentMeta[];
} | undefined {
  const rec = eventRecord(value);
  const messageId = nonBlankString(rec.messageId);
  if (!messageId || !Array.isArray(rec.attachments)) return undefined;
  for (const item of rec.attachments) {
    if (!isMessageAttachmentMeta(item)) return undefined;
  }
  return {
    messageId,
    attachments: rec.attachments.map((item: MessageAttachmentMeta) => ({
      partIndex: item.partIndex,
      cid: item.cid,
      kind: 'image',
    })),
  };
}

function handoffTarget(value: unknown): {
  agent: string;
  sessionId: string;
  toolCallId?: string;
  afterSeq?: number;
} | undefined {
  const handoff = eventRecord(value);
  const agent = nonBlankString(handoff.agent);
  const sessionId = nonBlankString(handoff.session_id);
  const toolCallId = nonBlankString(handoff.handoff_tool_call_id);
  const afterSeq = validSeq(handoff.after_seq) ? handoff.after_seq : undefined;
  return agent && sessionId ? { agent, sessionId, toolCallId, afterSeq } : undefined;
}

function parseCompactionOutcome(value: unknown): CompactionOutcome | undefined {
  const rec = eventRecord(value);
  const status = stringField(rec.outcome, 'status');
  if (status === 'compacted') return { status: 'compacted' };
  if (status === 'unchanged') {
    const detail = stringField(rec.outcome, 'detail');
    return { status: 'unchanged', detail };
  }
  return undefined;
}

const handlers: Record<string, CustomEventHandler> = {
  status: (callbacks, value) => callbacks.onStatus(stringField(value, 'text') || null),
  usage: (callbacks, value) => {
    if (isUsageData(value)) callbacks.onUsage(value);
  },
  tool_summary: (callbacks, value) => {
    const id = stringField(value, 'tool_call_id');
    const summary = stringField(value, 'markdown');
    if (id !== undefined && summary !== undefined) callbacks.onToolSummary(id, summary);
  },
  session_title_updated: (callbacks, value) => {
    const title = stringField(value, 'title');
    if (callbacks.isForeground !== false && title !== undefined) setDocumentTitle(title);
  },
  session_title_generation_failed: (callbacks, value) => {
    const error = stringField(value, 'error');
    console.error('session_title_generation_failed:', error);
    callbacks.onStatus(error || null);
  },
  session_history_warning: (callbacks, value) => {
    const message =
      stringField(value, 'message') || 'Session history could not be loaded completely';
    console.error('session_history_warning:', message);
    callbacks.onRunFailed(message);
  },
  session_attach_boundary: (callbacks, value) => {
    const raw = eventRecord(value).attached_seq;
    if (validSeq(raw)) {
      callbacks.onAttachBoundary?.(raw);
    }
  },
  session_handoff: (callbacks, value) => {
    const target = handoffTarget(value);
    if (!target || target.afterSeq === undefined) return;
    callbacks.onHandoff?.(target.agent, target.sessionId, target.afterSeq);
  },
  hitl_pending_approval: (callbacks, value) => {
    const toolCallId = stringField(value, 'tool_call_id');
    const summary = stringField(value, 'summary') || '';
    if (!toolCallId) return;
    callbacks.onHitlPendingApproval?.(toolCallId, summary);
  },
  message_attachments: (callbacks, value) => {
    const parsed = parseMessageAttachments(value);
    if (!parsed) return;
    callbacks.onMessageAttachments?.(parsed.messageId, parsed.attachments);
  },
  session_compacting_started: (callbacks, value) => {
    const compactionId = stringField(value, 'compaction_id');
    callbacks.onCompactingStarted?.(compactionId);
  },
  session_compacting_completed: (callbacks, value) => {
    const outcome = parseCompactionOutcome(value);
    if (!outcome) return;
    const compactionId = stringField(value, 'compaction_id');
    callbacks.onCompactingCompleted?.(outcome, compactionId);
  },
  session_compacting_failed: (callbacks, value) => {
    const error = stringField(value, 'error') || 'Compaction failed';
    const compactionId = stringField(value, 'compaction_id');
    callbacks.onCompactingFailed?.(error, compactionId);
  },
  tool_update: (callbacks, value) => {
    const patch = parseToolUpdateEvent(value);
    if (patch) callbacks.onToolUpdate?.(patch);
  },
};

export function handleHarnxCustomEvent(
  name: string,
  value: unknown,
  callbacks: HarnxCustomEventCallbacks
) {
  handlers[name]?.(callbacks, value);
}
