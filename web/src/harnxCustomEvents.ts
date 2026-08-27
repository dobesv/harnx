import type { UsageData } from './UsageContext';
import { setDocumentTitle } from './sessionTitle';

export interface HarnxCustomEventCallbacks {
  onStatus: (text: string | null) => void;
  onRunFailed: (message: string) => void;
  onUsage: (usage: UsageData) => void;
  onToolSummary: (id: string, summary: string) => void;
  onHandoff?: (agent: string, sessionId: string) => void;
  isRunActive: boolean;
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

function handoffTarget(value: unknown): [string, string] | undefined {
  const handoff = eventRecord(value);
  const agent = nonBlankString(handoff.agent);
  const sessionId = nonBlankString(handoff.session_id);
  return agent && sessionId ? [agent, sessionId] : undefined;
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
    if (!callbacks.isRunActive) return;
    const target = handoffTarget(value);
    if (target) callbacks.onHandoff?.(...target);
  },
};

export function handleHarnxCustomEvent(
  name: string,
  value: unknown,
  callbacks: HarnxCustomEventCallbacks
) {
  handlers[name]?.(callbacks, value);
}
