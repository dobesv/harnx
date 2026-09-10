import type { UsageData } from './UsageContext';
import { setDocumentTitle } from './sessionTitle';

/** Global map tracking consumed handoff markers to prevent re-navigation. */
const consumedHandoffs = new Map<string, Set<string>>();

/** Check if a handoff marker has been consumed for a source session. */
export function isHandoffConsumed(sourceSessionId: string, handoffToolCallId: string): boolean {
  const sessionSet = consumedHandoffs.get(sourceSessionId);
  return sessionSet?.has(handoffToolCallId) ?? false;
}

/** Mark a handoff marker as consumed for a source session. */
export function markHandoffConsumed(sourceSessionId: string, handoffToolCallId: string): void {
  let sessionSet = consumedHandoffs.get(sourceSessionId);
  if (!sessionSet) {
    sessionSet = new Set();
    consumedHandoffs.set(sourceSessionId, sessionSet);
  }
  sessionSet.add(handoffToolCallId);
}

export interface HarnxCustomEventCallbacks {
  onStatus: (text: string | null) => void;
  onRunFailed: (message: string) => void;
  onUsage: (usage: UsageData) => void;
  onToolSummary: (id: string, summary: string) => void;
  onHandoff?: (agent: string, sessionId: string) => void;
  /** Called when a hitl_pending_approval CUSTOM event is received. */
  onHitlPendingApproval?: (toolCallId: string, summary: string) => void;
  isRunActive: boolean;
  /** Source session ID for handoff deduplication (set by ChatProvider). */
  sourceSessionId?: string;
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

function handoffTarget(value: unknown): { agent: string; sessionId: string; toolCallId?: string } | undefined {
  const handoff = eventRecord(value);
  const agent = nonBlankString(handoff.agent);
  const sessionId = nonBlankString(handoff.session_id);
  const toolCallId = nonBlankString(handoff.handoff_tool_call_id);
  return agent && sessionId ? { agent, sessionId, toolCallId } : undefined;
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
  session_handoff: (callbacks, value) => {
    const target = handoffTarget(value);
    if (!target) return;

    const hasMarker = Boolean(target.toolCallId && callbacks.sourceSessionId);

    // On hydrated replay (!isRunActive), require both a non-blank toolCallId and
    // sourceSessionId. Without them we can't dedupe, so ignore to prevent
    // spurious re-navigation. Live events (isRunActive) always navigate.
    if (!callbacks.isRunActive && !hasMarker) return;

    // With a marker, dedupe: skip if already navigated, else record it.
    if (hasMarker) {
      if (isHandoffConsumed(callbacks.sourceSessionId!, target.toolCallId!)) return;
      markHandoffConsumed(callbacks.sourceSessionId!, target.toolCallId!);
    }

    callbacks.onHandoff?.(target.agent, target.sessionId);
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
