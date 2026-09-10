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
      this.options = options;
      this.runAgent = vi.fn();
      this.simulateEvent = (event: unknown) => options.onSubAgentEvent(event);
      this.simulateUsage = (usage: unknown) => options.onUsage(usage);
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

    it('constructs HarnxHttpAgent with session URL without /prompt suffix', () => {
      const runningNote: SubAgentNote = {
        ...note('running', 'child-123'),
        agent: 'researcher',
      };

      render(
        <SubAgentNotesContext.Provider value={{ notes: [], openSession: () => {}, dispatch: () => {} }}>
          <SubAgentSessionNotes notes={[runningNote]} onOpen={() => {}} />
        </SubAgentNotesContext.Provider>
      );

      expect(vi.mocked(HarnxHttpAgent)).toHaveBeenCalledWith(
        expect.objectContaining({
          url: '/v1/agents/researcher/sessions/child-123',
        })
      );
    });

    it('preserves accumulated elapsed and updatedAtMs when progress lacks startedAtMs', () => {
      const dispatch = vi.fn();
      const runningNote: SubAgentNote = {
        ...note('running', 'live-child'),
        elapsedMs: 2000,
        updatedAtMs: Date.now() - 500,
        startedAtMs: undefined,
      };

      render(
        <SubAgentNotesContext.Provider value={{ notes: [], openSession: () => {}, dispatch }}>
          <SubAgentSessionNotes notes={[runningNote]} onOpen={() => {}} />
        </SubAgentNotesContext.Provider>
      );

      const agentInstance = vi.mocked(HarnxHttpAgent).mock.instances[0] as any;
      agentInstance.simulateUsage({ input: 50, output: 25, cached: 10 });

      expect(dispatch).toHaveBeenCalledWith({
        type: 'CUSTOM',
        name: 'sub_agent_progress',
        value: expect.objectContaining({
          elapsed_ms: expect.any(Number),
          usage: { input_tokens: 50, output_tokens: 25, cached_tokens: 10 },
        }),
      });

      const call = dispatch.mock.calls[0][0];
      // Must be at least 2500ms (2000 accumulated + 500 elapsed since update), NOT 0
      expect(call.value.elapsed_ms).toBeGreaterThanOrEqual(2450);
    });

    it('computes elapsed from startedAtMs when present in onUsage', () => {
      const dispatch = vi.fn();
      const startTime = Date.now() - 3500;
      const runningNote: SubAgentNote = {
        ...note('running', 'live-child'),
        elapsedMs: 1000,
        updatedAtMs: Date.now() - 100,
        startedAtMs: startTime,
      };

      render(
        <SubAgentNotesContext.Provider value={{ notes: [], openSession: () => {}, dispatch }}>
          <SubAgentSessionNotes notes={[runningNote]} onOpen={() => {}} />
        </SubAgentNotesContext.Provider>
      );

      const agentInstance = vi.mocked(HarnxHttpAgent).mock.instances[0] as any;
      agentInstance.simulateUsage({ input: 50, output: 25, cached: 10 });

      const call = dispatch.mock.calls[0][0];
      // Must be computed from startedAtMs (~3500ms)
      expect(call.value.elapsed_ms).toBeGreaterThanOrEqual(3400);
      expect(call.value.started_at).toBe(startTime);
    });
  });
});
