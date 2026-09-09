import { fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it, vi, afterEach } from 'vitest';
import { SubAgentSessionNotes } from './SubAgentSessionNotes';
import type { SubAgentNote } from './subAgentNotes';
import { HarnxHttpAgent } from './ChatProvider';
import { SubAgentNotesContext } from './SubAgentNotesContext';

vi.mock('./ChatProvider', async (importOriginal) => {
  const actual = await importOriginal<typeof import('./ChatProvider')>();
  return {
    ...actual,
    HarnxHttpAgent: vi.fn().mockImplementation(function(this: any, options: any) {
      this.runAgent = vi.fn();
      this.simulateEvent = (event: unknown) => options.onSubAgentEvent(event);
    }),
  };
});

const note = (
  status: SubAgentNote['status'],
  sessionId = `child-session-${status}`,
): SubAgentNote => ({
  id: status,
  agent: 'researcher',
  sessionId,
  parentMessageId: 'assistant-1',
  status,
  elapsedMs: status === 'running' ? 1_000 : 2_500,
  inputTokens: 120,
  outputTokens: 45,
  cachedTokens: 30,
  toolCallCount: 3,
  updatedAtMs: Date.now(),
});

describe('SubAgentSessionNotes', () => {
  afterEach(() => {
    vi.clearAllMocks();
  });

  it('shows the full identity and running, done, and failed appearances', () => {
    const fullSessionId = '01948a3f-7b1c-7123-8901-abcdef123456';
    render(
      <SubAgentSessionNotes
        notes={[note('running', fullSessionId), note('done'), note('failed')]}
        onOpen={() => {}}
      />,
    );

    expect(screen.getByText(fullSessionId)).toBeVisible();
    expect(screen.getByText('Running').closest('button')).toHaveAttribute('data-status', 'running');
    expect(screen.getByText('Done').closest('button')).toHaveAttribute('data-status', 'done');
    expect(screen.getByText('Failed').closest('button')).toHaveAttribute('data-status', 'failed');
    expect(screen.getAllByText('in 120')).toHaveLength(3);
    expect(screen.getAllByText('out 45')).toHaveLength(3);
    expect(screen.getAllByText('cache 30')).toHaveLength(3);
    expect(screen.getAllByText('tools 3')).toHaveLength(3);
    expect(screen.getAllByText('2s')).toHaveLength(2);
    expect(screen.queryByText('2.5s')).not.toBeInTheDocument();
    expect(screen.getByRole('button', {
      name: `Open researcher sub-agent session ${fullSessionId} (running)`,
    })).toBeVisible();
  });

  it('opens a child session by click, Enter, or Space', () => {
    const onOpen = vi.fn();
    render(<SubAgentSessionNotes notes={[note('done')]} onOpen={onOpen} />);
    const button = screen.getByRole('button');

    fireEvent.click(button);
    fireEvent.keyDown(button, { key: 'Enter' });
    fireEvent.keyDown(button, { key: ' ' });

    expect(onOpen).toHaveBeenCalledTimes(3);
    expect(onOpen).toHaveBeenLastCalledWith('researcher', 'child-session-done');
  });

  describe('ChildMetricsSubscriber', () => {
    it('dispatches CHILD_TERMINAL with status done when RUN_FINISHED is received after grace period cleanly', () => {
      const dispatch = vi.fn();
      const runningNote: SubAgentNote = {
        ...note('running', 'live-child'),
        startedAtMs: Date.now() - 12000, // 12s ago, past 5s grace
      };

      render(
        <SubAgentNotesContext.Provider value={{ notes: [], openSession: () => {}, dispatch }}>
          <SubAgentSessionNotes notes={[runningNote]} onOpen={() => {}} />
        </SubAgentNotesContext.Provider>
      );

      const agentInstance = vi.mocked(HarnxHttpAgent).mock.instances[0] as any;
      agentInstance.simulateEvent({ type: 'RUN_FINISHED' });

      expect(dispatch).toHaveBeenCalledWith(
        expect.objectContaining({ type: 'CHILD_TERMINAL', status: 'done' })
      );
    });

    it('dispatches CHILD_TERMINAL with status failed when RUN_ERROR is received after grace period', () => {
      const dispatch = vi.fn();
      const runningNote: SubAgentNote = {
        ...note('running', 'live-child'),
        startedAtMs: Date.now() - 10000, // 10s ago, well past 5s grace
      };

      render(
        <SubAgentNotesContext.Provider value={{ notes: [], openSession: () => {}, dispatch }}>
          <SubAgentSessionNotes notes={[runningNote]} onOpen={() => {}} />
        </SubAgentNotesContext.Provider>
      );

      const agentInstance = vi.mocked(HarnxHttpAgent).mock.instances[0] as any;
      agentInstance.simulateEvent({ type: 'RUN_ERROR' });

      expect(dispatch).toHaveBeenCalledWith(
        expect.objectContaining({ type: 'CHILD_TERMINAL', status: 'failed' })
      );
    });

    it('ignores RUN_FINISHED when received during startup grace period', () => {
      const dispatch = vi.fn();
      const runningNote: SubAgentNote = {
        ...note('running', 'live-child'),
        startedAtMs: Date.now() - 1000, // 1s ago, within 5s grace
      };

      render(
        <SubAgentNotesContext.Provider value={{ notes: [], openSession: () => {}, dispatch }}>
          <SubAgentSessionNotes notes={[runningNote]} onOpen={() => {}} />
        </SubAgentNotesContext.Provider>
      );

      const agentInstance = vi.mocked(HarnxHttpAgent).mock.instances[0] as any;
      agentInstance.simulateEvent({ type: 'RUN_FINISHED' });

      expect(dispatch).not.toHaveBeenCalledWith(
        expect.objectContaining({ type: 'CHILD_TERMINAL' })
      );
    });
  });
});
