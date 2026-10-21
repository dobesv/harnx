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
import { CompactionMetadataContext } from '../CompactionMetadataContext';

function CompactionPhase() {
  const { phase } = useContext(CompactionContext);
  return <span data-testid="compaction-phase">{phase}</span>;
}

function CompactionMetadataView() {
  const metadata = useContext(CompactionMetadataContext);
  return (
    <div data-testid="compaction-metadata">
      {metadata ? (
        <>
          <span data-testid="compaction-msg-id">{metadata.message_id}</span>
          <span data-testid="compaction-seq">{metadata.compaction_seq}</span>
          <span data-testid="compaction-summary-text">{metadata.summary}</span>
        </>
      ) : (
        <span data-testid="no-compaction">none</span>
      )}
    </div>
  );
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

function renderProvider(child: ReactNode = <CompactionPhase />) {
  render(
    <ChatProvider agentName="agent" sessionId="session" isFreshSession onOpenSubAgent={() => {}}>
      {child}
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

  it('updates compaction metadata on session_compaction CUSTOM event and clears on null', async () => {
    renderProvider(<CompactionMetadataView />);
    expect(screen.getByTestId('no-compaction')).toBeInTheDocument();

    const subscriber = await startRun();

    // 1. Receive session_compaction with metadata
    await dispatch(subscriber, {
      type: 'CUSTOM',
      name: 'session_compaction',
      value: {
        compaction: {
          message_id: 'a0b1fe9f-c77c-568a-8ffc-7272dd4a5cc3',
          compaction_seq: 5,
          summary: 'The session log stores tool calls and results separately. No messages were kept.',
          from_seq: 1,
          to_seq: 4,
          kept_count: 0,
        },
      },
    });

    expect(screen.getByTestId('compaction-msg-id').textContent).toBe(
      'a0b1fe9f-c77c-568a-8ffc-7272dd4a5cc3',
    );
    expect(screen.getByTestId('compaction-seq').textContent).toBe('5');
    expect(screen.getByTestId('compaction-summary-text').textContent).toBe(
      'The session log stores tool calls and results separately. No messages were kept.',
    );

    // 2. Receive session_compaction with null (clears marker)
    await dispatch(subscriber, {
      type: 'CUSTOM',
      name: 'session_compaction',
      value: {
        compaction: null,
      },
    });

    expect(screen.getByTestId('no-compaction')).toBeInTheDocument();
  });
});
