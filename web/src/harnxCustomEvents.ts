import type { UsageData } from './UsageContext';
import { setDocumentTitle } from './sessionTitle';
import type { ToolCallUpdatePatch } from './toolUpdates';
import { parseToolUpdateEvent } from './toolUpdates';
import { parseCompactionMetadata, type CompactionMetadata } from './compactionMetadata';
export { parseCompactionMetadata } from './compactionMetadata';
import type { A2uiSurfaceEvent, A2uiSurfaceResolution } from './a2ui/surfaceState';
export type { A2uiSurfaceEvent, A2uiSurfaceResolution } from './a2ui/surfaceState';

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
  'session_compaction',
  'session_compacting_started',
  'session_compacting_completed',
  'session_compacting_failed',
  'pending_interaction',
  'a2ui_surface',
] as const;

export interface PendingInteraction {
  interactionId: string;
  requestSeq: number;
  toolCallId: string;
  kind: string;
  message: string;
  pendingCount: number;
  payload?: unknown;
  /** Present for `url` interactions: the https or loopback-http link the user opens (decision 6). */
  url?: string;
}

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
  /** Called when session_compaction CUSTOM event is received. */
  onSessionCompaction?: (compaction: CompactionMetadata | null) => void;
  /** Called when a tool_update CUSTOM event is received. */
  onToolUpdate?: (patch: ToolCallUpdatePatch) => void;
  /** Called when a pending_interaction CUSTOM event is received (or null to clear). */
  onPendingInteraction?: (interaction: PendingInteraction | null) => void;
  /** Called when an a2ui_surface CUSTOM event is received. */
  onA2uiSurface?: (surface: A2uiSurfaceEvent) => void;
  /** Called when turn_interrupted is received. */
  onTurnInterrupted?: () => void;
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

export function parsePendingInteraction(value: unknown): PendingInteraction | null | undefined {
  if (value === null) return null;
  if (typeof value !== 'object') return undefined;
  const rec = value as Record<string, unknown>;
  const interactionId = nonBlankString(rec.interaction_id);
  const toolCallId = nonBlankString(rec.tool_call_id);
  const kind = typeof rec.kind === 'string' && rec.kind.trim().length > 0 ? rec.kind : undefined;
  const message = typeof rec.message === 'string' ? rec.message : undefined;
  const requestSeq = validSeq(rec.request_seq) && rec.request_seq > 0 ? rec.request_seq : undefined;
  const pendingCount =
    typeof rec.pending_count === 'number' &&
    Number.isSafeInteger(rec.pending_count) &&
    rec.pending_count >= 0
      ? rec.pending_count
      : undefined;

  if (
    interactionId !== undefined &&
    toolCallId !== undefined &&
    kind !== undefined &&
    message !== undefined &&
    requestSeq !== undefined &&
    pendingCount !== undefined
  ) {
    const interaction: PendingInteraction = {
      interactionId,
      requestSeq,
      toolCallId,
      kind,
      message,
      pendingCount,
    };
    if (rec.payload !== undefined) {
      interaction.payload = rec.payload;
    }
    if (kind === 'url' && typeof rec.url === 'string' && rec.url.length > 0) {
      interaction.url = rec.url;
    } else if (kind === 'url') {
      const payloadUrl = eventRecord(rec.payload).url;
      if (typeof payloadUrl === 'string' && payloadUrl.length > 0) {
        interaction.url = payloadUrl;
      }
    }
    return interaction;
  }
  return undefined;
}

export function parseA2uiSurface(value: unknown): A2uiSurfaceEvent | undefined {
  if (value === null || typeof value !== 'object') return undefined;
  const rec = value as Record<string, unknown>;
  const surfaceId = nonBlankString(rec.surface_id);
  const publishSeq =
    typeof rec.publish_seq === 'number' &&
    Number.isSafeInteger(rec.publish_seq) &&
    rec.publish_seq > 0
      ? rec.publish_seq
      : undefined;

  if (surfaceId === undefined || publishSeq === undefined) {
    return undefined;
  }

  const toolCallId = nonBlankString(rec.tool_call_id);
  const payload = rec.payload;

  let action: A2uiSurfaceResolution | null | undefined = undefined;

  if (rec.action !== undefined) {
    if (rec.action === null) {
      action = null;
    } else if (typeof rec.action === 'object') {
      const actRec = rec.action as Record<string, unknown>;
      const actionName = nonBlankString(actRec.action);
      if (actionName) {
        action = {
          action: actionName,
          visibility: nonBlankString(actRec.visibility),
          noteText: nonBlankString(actRec.note_text ?? actRec.noteText),
        };
      }
    } else if (typeof rec.action === 'string' && rec.action.trim().length > 0) {
      action = {
        action: rec.action.trim(),
        visibility: nonBlankString(rec.visibility),
        noteText: nonBlankString(rec.note_text ?? rec.noteText),
      };
    }
  } else if (typeof rec.action === 'string' && (rec.action as string).trim().length > 0) {
    action = {
      action: (rec.action as string).trim(),
      visibility: nonBlankString(rec.visibility),
      noteText: nonBlankString(rec.note_text ?? rec.noteText),
    };
  }

  // Must carry either payload or action resolution to be a valid surface event
  if (payload === undefined && action === undefined) {
    return undefined;
  }

  const result: A2uiSurfaceEvent = {
    surfaceId,
    publishSeq,
  };
  if (toolCallId !== undefined) {
    result.toolCallId = toolCallId;
  }
  if (payload !== undefined) {
    result.payload = payload;
  }
  if (action !== undefined) {
    result.action = action;
  }

  return result;
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
  session_compaction: (callbacks, value) => {
    const rec = eventRecord(value);
    const compaction = parseCompactionMetadata(rec.compaction);
    callbacks.onSessionCompaction?.(compaction);
  },
  tool_update: (callbacks, value) => {
    const patch = parseToolUpdateEvent(value);
    if (patch) callbacks.onToolUpdate?.(patch);
  },
  pending_interaction: (callbacks, value) => {
    const parsed = parsePendingInteraction(value);
    if (parsed !== undefined) {
      callbacks.onPendingInteraction?.(parsed);
    }
  },
  a2ui_surface: (callbacks, value) => {
    const parsed = parseA2uiSurface(value);
    if (parsed) {
      callbacks.onA2uiSurface?.(parsed);
    }
  },
  turn_interrupted: (callbacks) => {
    callbacks.onTurnInterrupted?.();
  },
};

export function handleHarnxCustomEvent(
  name: string,
  value: unknown,
  callbacks: HarnxCustomEventCallbacks
) {
  handlers[name]?.(callbacks, value);
}
