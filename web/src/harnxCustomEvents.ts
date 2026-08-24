import type { UsageData } from './UsageContext';
import { setDocumentTitle } from './sessionTitle';

export interface HarnxCustomEventCallbacks {
  onStatus: (text: string | null) => void;
  onRunFailed: (message: string) => void;
  onUsage: (usage: UsageData) => void;
  onToolSummary: (id: string, summary: string) => void;
  onHandoff?: (agent: string, sessionId: string | null) => void;
  isRunActive: boolean;
}

type CustomEventHandler = (callbacks: HarnxCustomEventCallbacks, value: any) => void;

const handlers: Record<string, CustomEventHandler> = {
  status: (callbacks, value) => callbacks.onStatus(value?.text || null),
  usage: (callbacks, value) => callbacks.onUsage(value),
  tool_summary: (callbacks, value) =>
    callbacks.onToolSummary(value?.tool_call_id, value?.markdown),
  session_title_updated: (_callbacks, value) => setDocumentTitle(value?.title),
  session_title_generation_failed: (callbacks, value) => {
    console.error('session_title_generation_failed:', value?.error);
    callbacks.onStatus(value?.error || null);
  },
  session_history_warning: (callbacks, value) => {
    const message = value?.message || 'Session history could not be loaded completely';
    console.error('session_history_warning:', message);
    callbacks.onRunFailed(message);
  },
  session_handoff: (callbacks, value) => {
    if (callbacks.isRunActive) {
      callbacks.onHandoff?.(value?.agent, value?.session_id ?? null);
    }
  },
};

export function handleHarnxCustomEvent(
  name: string,
  value: any,
  callbacks: HarnxCustomEventCallbacks
) {
  handlers[name]?.(callbacks, value);
}
