import { describe, it, expect, vi } from 'vitest';
import { act, render, screen } from '@testing-library/react';
import { useContext, type ReactNode } from 'react';

let capturedAgent: any;
vi.mock('@assistant-ui/react-ag-ui', () => ({
  useAgUiRuntime: ({ agent }: { agent: unknown }) => {
    capturedAgent = agent;
    return {};
  },
}));
vi.mock('@assistant-ui/react', () => ({
  AssistantRuntimeProvider: ({ children }: { children: ReactNode }) => <>{children}</>,
}));
vi.mock('../RuntimeSessionSubscriber', () => ({ RuntimeSessionSubscriber: () => null }));

import { ChatProvider } from '../ChatProvider';
import { CompactionContext } from '../CompactionContext';

function CompactionPhase() {
  const { phase } = useContext(CompactionContext);
  return <span data-testid="compaction-phase">{phase}</span>;
}

/** Start a run on the provider's agent and return the subscriber it streams events to. */
async function startRun(): Promise<any> {
  const subscriber: any = {};
  vi.spyOn(Object.getPrototypeOf(Object.getPrototypeOf(capturedAgent)), 'runAgent').mockImplementation(
    (_params: any, sub: any) => {
      Object.assign(subscriber, sub);
      return Promise.resolve();
    },
  );
  await capturedAgent.runAgent({});
  return subscriber;
}

async function dispatch(subscriber: any, event: any) {
  await act(async () => {
    await subscriber.onEvent({ event });
  });
}

function renderProvider() {
  render(
    <ChatProvider agentName="agent" sessionId="session" isFreshSession onOpenSubAgent={() => {}}>
      <CompactionPhase />
    </ChatProvider>,
  );
}

const phase = () => screen.getByTestId('compaction-phase').textContent;

describe('ChatProvider compaction phase', () => {
  it('settles an automatic compaction when its run ends without a completion event', async () => {
    renderProvider();
    const subscriber = await startRun();

    await dispatch(subscriber, { type: 'CUSTOM', name: 'session_compacting_started', value: {} });
    expect(phase()).toBe('compacting');

    await dispatch(subscriber, { type: 'RUN_FINISHED' });
    expect(phase()).toBe('idle');
  });

  it('keeps a manual compaction running past the end of a run', async () => {
    renderProvider();
    const subscriber = await startRun();

    await dispatch(subscriber, {
      type: 'CUSTOM',
      name: 'session_compacting_started',
      value: { compaction_id: 'compact-1' },
    });
    await dispatch(subscriber, { type: 'RUN_FINISHED' });
    expect(phase()).toBe('compacting');
  });
});
