import { useCallback, useState } from 'react';
import { EMPTY_PENDING_PROMPTS, updateSessionPendingPrompts, type PendingPromptsState } from './pendingPrompts';

export function usePendingSessionPrompts(agent: string | null, session: string | null) {
  const [bySession, setBySession] = useState<Record<string, PendingPromptsState>>({});
  const key = JSON.stringify([agent, session]);
  const updatePendingState = useCallback((update: (current: PendingPromptsState) => PendingPromptsState) => {
    setBySession((current) => updateSessionPendingPrompts(current, key, update));
  }, [key]);

  return {
    pendingState: bySession[key] ?? EMPTY_PENDING_PROMPTS,
    updatePendingState,
  };
}
