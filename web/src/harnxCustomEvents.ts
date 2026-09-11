import type { UsageData } from './UsageContext';
import { setDocumentTitle } from './sessionTitle';

/**
 * Internal Harnx control events that drive navigation and sequence gating,
 * but must NOT be forwarded to the assistant-ui runtime (they are not chat content).
 */
export const NAVIGATION_CONTROL_EVENTS = ['session_attach_boundary', 'session_handoff'] as const;

export interface HarnxCustomEventCallbacks {
  onStatus: (text: string | null) => void;
  onRunFailed: (message: string) => void;
  onUsage: (usage: UsageData) => void;
  onToolSummary: (id: string, summary: string) => void;
  onAttachBoundary?: (seq: number) => void;
  onHandoff?: (agent: string, sessionId: string, afterSeq: number) => void;
  /** Called when a hitl_pending_approval CUSTOM event is received. */
  onHitlPendingApproval?: (toolCallId: string, summary: string) => void;
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
  session_title_updated: (_callbacks, value) => {
    const title = stringField(value, 'title');
    if (title !== undefined) setDocumentTitle(title);
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
};

export function handleHarnxCustomEvent(
  name: string,
  value: unknown,
  callbacks: HarnxCustomEventCallbacks
) {
  handlers[name]?.(callbacks, value);
}
