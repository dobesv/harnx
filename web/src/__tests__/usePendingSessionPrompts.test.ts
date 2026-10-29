import { act, renderHook } from '@testing-library/react';
import { describe, expect, it } from 'vitest';
import { createPendingPrompt } from '../pendingPrompts';
import { usePendingSessionPrompts } from '../usePendingSessionPrompts';

describe('session pending prompts', () => {
  it('routes a late acknowledgement to its original agent and session', () => {
    const { result, rerender } = renderHook(
      ({ agent, session }) => usePendingSessionPrompts(agent, session),
      { initialProps: { agent: 'first-agent', session: 'first-session' } },
    );
    const acknowledgeFirst = result.current.updatePendingState;
    const first = createPendingPrompt('first', [], 'server-first');
    const second = createPendingPrompt('second', [], 'server-second');

    rerender({ agent: 'first-agent', session: 'second-session' });
    act(() => result.current.updatePendingState(() => ({ prompts: [second] })));
    act(() => acknowledgeFirst((current) => ({ prompts: [...current.prompts, first] })));
    expect(result.current.pendingState.prompts).toEqual([second]);

    rerender({ agent: 'first-agent', session: 'first-session' });
    expect(result.current.pendingState.prompts).toEqual([first]);
    rerender({ agent: 'other-agent', session: 'first-session' });
    expect(result.current.pendingState.prompts).toEqual([]);
  });
});
