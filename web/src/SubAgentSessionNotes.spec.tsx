import { act, fireEvent, render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { useReducer } from 'react';
import { SubAgentSessionNotes } from './SubAgentSessionNotes';
import { cancel } from './api';
vi.mock('./api', () => ({ cancel: vi.fn() }));

import {
  INITIAL_SUB_AGENT_NOTES_STATE,
  reduceSubAgentNotes,
  type SubAgentNote,
} from './subAgentNotes';
import { HarnxHttpAgent } from './ChatProvider';
import { SubAgentNotesContext } from './SubAgentNotesContext';

// Mirror the real agent's custom-event dispatch to the typed callbacks so a
// simulated CUSTOM event reaches onTurnInterrupted / onHitlPendingApproval.
function dispatchSimulatedCustomEvent(options: any, event: any) {
  if (event?.type !== 'CUSTOM') return;
  if (event.name === 'turn_interrupted') options.onTurnInterrupted?.();
  if (event.name === 'hitl_pending_approval') {
    options.onHitlPendingApproval?.(event.value?.tool_call_id, event.value?.summary);
  }
}

vi.mock('./ChatProvider', async (importOriginal) => {
  const actual = await importOriginal<typeof import('./ChatProvider')>();
  return {
    ...actual,
    HarnxHttpAgent: vi.fn().mockImplementation(function(this: any, options: any) {
      this.options = options;
      this.runAgent = vi.fn();
      this.simulateEvent = (event: any) => {
        options.onSubAgentEvent(event);
        dispatchSimulatedCustomEvent(options, event);
      };
      this.simulateUsage = (usage: unknown) => options.onUsage(usage);
      this.simulateToolSummary = (id = 'call-1', summary = 'tool-summary') => options.onToolSummary(id, summary);
      this.simulateHitlPendingApproval = (toolCallId = 'call-1', summary = 'approval required') =>
        options.onHitlPendingApproval?.(toolCallId, summary);
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
    vi.useRealTimers();
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
    expect(screen.getByText('Running').closest('.aui-sub-agent-row')).toHaveAttribute('data-status', 'running');
    expect(screen.getByText('Done').closest('.aui-sub-agent-row')).toHaveAttribute('data-status', 'done');
    expect(screen.getByText('Failed').closest('.aui-sub-agent-row')).toHaveAttribute('data-status', 'failed');
    expect(screen.getAllByText('in 120')).toHaveLength(3);
    expect(screen.getAllByText('out 45')).toHaveLength(3);
    expect(screen.getAllByText('cache 30')).toHaveLength(3);
    expect(screen.getAllByText('tools 3')).toHaveLength(3);
    expect(screen.getAllByText('2s')).toHaveLength(2);
    expect(screen.queryByText('2.5s')).not.toBeInTheDocument();
    const runningLink = screen.getByRole('link', {
      name: `Open researcher sub-agent session ${fullSessionId} (running)`,
    });
    expect(runningLink).toBeVisible();
    expect(runningLink).toHaveAttribute('href', `/agents/researcher/sessions/${fullSessionId}`);
  });

  it('opens a child session by click or Enter, but not Space', async () => {
    const user = userEvent.setup();
    const onOpen = vi.fn();
    render(<SubAgentSessionNotes notes={[note('done')]} onOpen={onOpen} />);
    const link = screen.getByRole('link', {
      name: 'Open researcher sub-agent session child-session-done (done)',
    });

    await user.click(link);
    expect(onOpen).toHaveBeenCalledTimes(1);
    expect(onOpen).toHaveBeenLastCalledWith('researcher', 'child-session-done');

    link.focus();
    await user.keyboard('{Enter}');
    expect(onOpen).toHaveBeenCalledTimes(2);
    expect(onOpen).toHaveBeenLastCalledWith('researcher', 'child-session-done');

    await user.keyboard(' ');
    expect(onOpen).toHaveBeenCalledTimes(2);
  });

  it('renders open session affordance as an anchor link with properly encoded href', () => {
    const customNote: SubAgentNote = {
      ...note('done', 'child/session?#1'),
      agent: 'special/agent',
    };
    render(<SubAgentSessionNotes notes={[customNote]} onOpen={() => {}} />);
    const link = screen.getByRole('link');
    expect(link).toHaveAttribute(
      'href',
      '/agents/special%2Fagent/sessions/child%2Fsession%3F%231',
    );
  });

  describe('subagent title', () => {
    it('renders the title element when note.title is set', () => {
      render(
        <SubAgentSessionNotes
          notes={[{ ...note('running'), title: 'Searching codebase for patterns' }]}
          onOpen={() => {}}
        />,
      );

      const titleEl = screen.getByText('Searching codebase for patterns');
      expect(titleEl).toBeVisible();
      expect(titleEl).toHaveClass('aui-sub-agent-title');
      expect(titleEl).toHaveAttribute('title', 'Searching codebase for patterns');
    });

    it('does not render the title element when note.title is undefined', () => {
      const { container } = render(
        <SubAgentSessionNotes
          notes={[note('running')]}
          onOpen={() => {}}
        />,
      );

      expect(container.querySelector('.aui-sub-agent-title')).not.toBeInTheDocument();
    });

    it('does not render the title element when note.title is empty or whitespace', () => {
      const { container } = render(
        <SubAgentSessionNotes
          notes={[
            { ...note('running', 'child-empty'), title: '' },
            { ...note('done', 'child-whitespace'), title: '   ' },
          ]}
          onOpen={() => {}}
        />,
      );

      expect(container.querySelector('.aui-sub-agent-title')).not.toBeInTheDocument();
    });

    it('updates the title when new progress arrives with an updated title', () => {
      const initialNote: SubAgentNote = {
        ...note('running'),
        title: 'Initial Title',
      };
      const { rerender } = render(
        <SubAgentSessionNotes notes={[initialNote]} onOpen={() => {}} />,
      );

      expect(screen.getByText('Initial Title')).toBeVisible();

      const updatedNote: SubAgentNote = {
        ...initialNote,
        title: 'Updated Progress Title',
      };
      rerender(<SubAgentSessionNotes notes={[updatedNote]} onOpen={() => {}} />);

      expect(screen.queryByText('Initial Title')).not.toBeInTheDocument();
      const updatedEl = screen.getByText('Updated Progress Title');
      expect(updatedEl).toBeVisible();
      expect(updatedEl).toHaveAttribute('title', 'Updated Progress Title');
    });

    it('defines CSS truncation and ellipsis styling for .aui-sub-agent-title in chat.css', async () => {
      const fsMod = 'node:fs';
      const pathMod = 'node:path';
      const fs = (await import(/* @vite-ignore */ fsMod)) as unknown as {
        readFileSync: (file: string, encoding: string) => string;
      };
      const path = (await import(/* @vite-ignore */ pathMod)) as unknown as {
        resolve: (...parts: string[]) => string;
      };
      const g = globalThis as unknown as { process?: { cwd: () => string } };
      const cwd = g.process ? g.process.cwd() : '.';
      const css = fs.readFileSync(path.resolve(cwd, 'src/chat.css'), 'utf8');
      const ruleMatch = css.match(/\.aui-sub-agent-title\s*\{([^}]+)\}/);
      expect(ruleMatch).not.toBeNull();
      const body = ruleMatch ? ruleMatch[1] : '';
      expect(body).toMatch(/text-overflow:\s*ellipsis;/);
      expect(body).toMatch(/overflow:\s*hidden;/);
      expect(body).toMatch(/white-space:\s*nowrap;/);
      expect(body).toMatch(/min-width:\s*0;/);
      expect(body).toMatch(/max-width:\s*100%;/);
    });
  });

  describe('ChildMetricsSubscriber', () => {
    function setupSubscriber(customNote?: SubAgentNote) {
      const dispatch = vi.fn();
      const testNote: SubAgentNote = customNote ?? {
        ...note('running', 'live-child'),
        startedAtMs: Date.now() - 10000,
      };

      render(
        <SubAgentNotesContext.Provider value={{ notes: [], openSession: () => {}, dispatch }}>
          <SubAgentSessionNotes notes={[testNote]} onOpen={() => {}} />
        </SubAgentNotesContext.Provider>
      );

      const instances = vi.mocked(HarnxHttpAgent).mock.instances;
      const agentInstance = instances[instances.length - 1] as any;
      return { dispatch, agentInstance };
    }

    it.each([
      { event: { type: 'RUN_FINISHED' }, expectedType: 'CHILD_TERMINAL', expectedStatus: 'done' },
      { event: { type: 'RUN_ERROR' }, expectedType: 'CHILD_TERMINAL', expectedStatus: 'failed' },
      { event: { type: 'CUSTOM', name: 'turn_interrupted' }, expectedType: 'CHILD_TERMINAL', expectedStatus: 'cancelled' },
      {
        event: {
          type: 'CUSTOM',
          name: 'hitl_pending_approval',
          value: { tool_call_id: 'call-1', summary: 'Need confirmation' },
        },
        expectedType: 'CHILD_AWAITING_APPROVAL',
      },
    ])(
      'dispatches $expectedType ($expectedStatus) on $event.type ($event.name)',
      ({ event, expectedType, expectedStatus }) => {
        const { dispatch, agentInstance } = setupSubscriber();
        agentInstance.simulateEvent(event);

        expect(dispatch).toHaveBeenCalledWith(
          expect.objectContaining({
            type: expectedType,
            ...(expectedStatus ? { status: expectedStatus } : {}),
          })
        );
      },
    );

    it('dispatches CHILD_TERMINAL with status cancelled when RUN_FINISHED arrives after turn_interrupted', () => {
      const { dispatch, agentInstance } = setupSubscriber();
      agentInstance.simulateEvent({ type: 'CUSTOM', name: 'turn_interrupted' });
      dispatch.mockClear();

      agentInstance.simulateEvent({ type: 'RUN_FINISHED' });
      expect(dispatch).toHaveBeenCalledWith(
        expect.objectContaining({ type: 'CHILD_TERMINAL', status: 'cancelled' })
      );
    });

    it('dispatches CHILD_AWAITING_APPROVAL when onHitlPendingApproval is called', () => {
      const { dispatch, agentInstance } = setupSubscriber();
      agentInstance.simulateHitlPendingApproval('call-1', 'Need confirmation');

      expect(dispatch).toHaveBeenCalledWith(
        expect.objectContaining({ type: 'CHILD_AWAITING_APPROVAL' })
      );
    });

    it('dispatches CHILD_RUNNING when RUN_STARTED is received while awaiting approval', () => {
      const { dispatch, agentInstance } = setupSubscriber({
        ...note('awaiting_approval', 'live-child'),
        startedAtMs: Date.now() - 10000,
      });
      agentInstance.simulateEvent({ type: 'RUN_STARTED' });

      expect(dispatch).toHaveBeenCalledWith(
        expect.objectContaining({ type: 'CHILD_RUNNING' })
      );
    });

    it('defers rather than loses RUN_FINISHED received during startup', () => {
      vi.useFakeTimers();
      const now = Date.now();
      const dispatch = vi.fn();
      const runningNote: SubAgentNote = {
        ...note('running', 'live-child'),
        startedAtMs: now - 4_999,
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
      act(() => vi.advanceTimersByTime(1));
      expect(dispatch).toHaveBeenCalledWith(
        expect.objectContaining({ type: 'CHILD_TERMINAL', status: 'done' })
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

    it('preserves counters and elapsed across successive usage ticks without recreating subscriber', () => {
      const dispatch = vi.fn();
      const startTime = Date.now() - 2000;
      let runningNote: SubAgentNote = {
        ...note('running', 'live-child'),
        elapsedMs: 2000,
        updatedAtMs: Date.now() - 500,
        startedAtMs: startTime,
        toolCallCount: 1,
      };

      const { rerender } = render(
        <SubAgentNotesContext.Provider value={{ notes: [], openSession: () => {}, dispatch }}>
          <SubAgentSessionNotes notes={[runningNote]} onOpen={() => {}} />
        </SubAgentNotesContext.Provider>
      );

      const agentInstance = vi.mocked(HarnxHttpAgent).mock.instances[0] as any;
      expect(vi.mocked(HarnxHttpAgent).mock.instances).toHaveLength(1);

      // Simulate a tool call occurring
      agentInstance.simulateToolSummary();

      // First usage tick
      agentInstance.simulateUsage({ input: 50, output: 25, cached: 10 });

      expect(dispatch).toHaveBeenCalledTimes(1);
      const firstCall = dispatch.mock.calls[0][0];
      expect(firstCall.value.tool_call_count).toBe(2);
      expect(firstCall.value.elapsed_ms).toBeGreaterThanOrEqual(1950);

      // Simulate parent component updating state and re-rendering with new note
      runningNote = {
        ...runningNote,
        elapsedMs: firstCall.value.elapsed_ms,
        toolCallCount: firstCall.value.tool_call_count,
        updatedAtMs: Date.now(),
      };

      rerender(
        <SubAgentNotesContext.Provider value={{ notes: [], openSession: () => {}, dispatch }}>
          <SubAgentSessionNotes notes={[runningNote]} onOpen={() => {}} />
        </SubAgentNotesContext.Provider>
      );

      // HarnxHttpAgent instance must NOT have been recreated
      expect(vi.mocked(HarnxHttpAgent).mock.instances).toHaveLength(1);

      // Another tool call occurring
      agentInstance.simulateToolSummary();

      // Second usage tick
      agentInstance.simulateUsage({ input: 80, output: 40, cached: 20 });

      expect(dispatch).toHaveBeenCalledTimes(2);
      const secondCall = dispatch.mock.calls[1][0];
      // Counter must be 3 (accumulated across ticks, not reset to 0 or 1)
      expect(secondCall.value.tool_call_count).toBe(3);
      // Elapsed must be >= first call's elapsed (preserved and progressing, not reset)
      expect(secondCall.value.elapsed_ms).toBeGreaterThanOrEqual(firstCall.value.elapsed_ms);
    });

    it('uses startedAtMs consistently in displayed elapsed time when running', () => {
      const startTime = Date.now() - 4000;
      const runningNote: SubAgentNote = {
        ...note('running', 'live-child'),
        elapsedMs: 1000, // Stale/initial elapsedMs
        updatedAtMs: Date.now(),
        startedAtMs: startTime,
      };

      const { container } = render(
        <SubAgentSessionNotes notes={[runningNote]} onOpen={() => {}} />
      );

      // Displayed elapsed time must reflect startedAtMs (~4s), not stale elapsedMs (1s)
      const row = container.querySelector('.aui-sub-agent-row');
      expect(row).toHaveAttribute('data-elapsed-ms');
      const elapsedAttr = Number(row?.getAttribute('data-elapsed-ms'));
      expect(elapsedAttr).toBeGreaterThanOrEqual(3900);
    });
  });
});


describe('child cancellation', () => {
  it('stops the running child session without opening it', async () => {
    vi.mocked(cancel).mockResolvedValue({ outcome: 'accepted', cancel_seq: 12 });
    const onOpen = vi.fn();
    render(<SubAgentSessionNotes notes={[{ ...note('running'), invocationId: 'invocation' }]} onOpen={onOpen} />);
    fireEvent.click(await screen.findByRole('button', { name: 'Stop researcher sub-agent session child-session-running' }));
    expect(onOpen).not.toHaveBeenCalled();
    expect(cancel).toHaveBeenCalledWith('researcher', 'child-session-running');
    expect(screen.getByText('Cancelling')).toBeVisible();
    expect(screen.queryByRole('button', { name: /^Stop / })).not.toBeInTheDocument();
  });

  it('offers Stop from the parent progress note for a running subagent', async () => {
    render(<SubAgentSessionNotes notes={[{ ...note('running'), invocationId: 'invocation' }]} onOpen={() => {}} />);
    expect(await screen.findByRole('button', { name: /^Stop / })).toBeEnabled();
  });

  it('hides Stop once the child session is cancelled', async () => {
    render(<SubAgentSessionNotes notes={[{ ...note('cancelled'), invocationId: 'invocation' }]} onOpen={() => {}} />);
    expect(await screen.findByText('Cancelled')).toBeVisible();
    expect(screen.queryByRole('button', { name: /^Stop / })).not.toBeInTheDocument();
  });

  it('labels a child parked at an approval gate without calling it cancelled', async () => {
    render(<SubAgentSessionNotes notes={[{ ...note('awaiting_approval'), invocationId: 'invocation' }]} onOpen={() => {}} />);
    expect(await screen.findByText('Awaiting approval')).toBeVisible();
    expect(screen.queryByText('Cancelled')).not.toBeInTheDocument();
  });

  it('does not surface "signal is aborted without reason" on screen (#1838)', async () => {
    vi.mocked(cancel).mockRejectedValue(new DOMException('signal is aborted without reason', 'AbortError'));
    render(<SubAgentSessionNotes notes={[{ ...note('running'), invocationId: 'invocation' }]} onOpen={() => {}} />);
    fireEvent.click(await screen.findByRole('button', { name: 'Stop researcher sub-agent session child-session-running' }));
    expect(await screen.findByText('Unconfirmed')).toBeVisible();
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
  });

  it('does not surface TimeoutError on screen (#1838, #1861)', async () => {
    vi.mocked(cancel).mockRejectedValue(new DOMException('The operation was aborted due to timeout', 'TimeoutError'));
    render(<SubAgentSessionNotes notes={[{ ...note('running'), invocationId: 'invocation' }]} onOpen={() => {}} />);
    fireEvent.click(await screen.findByRole('button', { name: 'Stop researcher sub-agent session child-session-running' }));
    expect(await screen.findByText('Unconfirmed')).toBeVisible();
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
  });

  it('surfaces legitimate non-abort errors on screen', async () => {
    vi.mocked(cancel).mockRejectedValue(new Error('Server communication failed'));
    render(<SubAgentSessionNotes notes={[{ ...note('running'), invocationId: 'invocation' }]} onOpen={() => {}} />);
    fireEvent.click(await screen.findByRole('button', { name: 'Stop researcher sub-agent session child-session-running' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('Error: Server communication failed');
  });

  describe('row status driven purely by events', () => {
    function EventDrivenNotes({ initialNotes }: { initialNotes: SubAgentNote[] }) {
      const [state, dispatch] = useReducer(reduceSubAgentNotes, {
        ...INITIAL_SUB_AGENT_NOTES_STATE,
        notes: initialNotes,
      });
      return (
        <SubAgentNotesContext.Provider value={{ notes: state.notes, openSession: () => {}, dispatch }}>
          <SubAgentSessionNotes notes={state.notes} onOpen={() => {}} />
        </SubAgentNotesContext.Provider>
      );
    }

    it('transitions Running -> Awaiting approval -> Running -> Cancelled purely via events', async () => {
      const initialNote: SubAgentNote = {
        ...note('running', 'event-child-1'),
        invocationId: 'inv-ev-1',
        toolCallId: 'call-ev-1',
        startedAtMs: Date.now() - 10000,
      };

      render(<EventDrivenNotes initialNotes={[initialNote]} />);

      expect(screen.getByText('Running')).toBeVisible();

      const agentInstance = vi.mocked(HarnxHttpAgent).mock.instances[0] as any;
      act(() => {
        agentInstance.simulateHitlPendingApproval('call-ev-1', 'Approval required');
      });
      expect(await screen.findByText('Awaiting approval')).toBeVisible();
      expect(screen.queryByText('Running')).not.toBeInTheDocument();

      act(() => {
        agentInstance.simulateEvent({ type: 'RUN_STARTED' });
      });
      expect(await screen.findByText('Running')).toBeVisible();
      expect(screen.queryByText('Awaiting approval')).not.toBeInTheDocument();

      act(() => {
        agentInstance.simulateEvent({ type: 'CUSTOM', name: 'turn_interrupted' });
      });
      expect(await screen.findByText('Cancelled')).toBeVisible();
      expect(screen.queryByText('Running')).not.toBeInTheDocument();
    });

    it('transitions Running -> Done on RUN_FINISHED event', async () => {
      const initialNote: SubAgentNote = {
        ...note('running', 'event-child-2'),
        invocationId: 'inv-ev-2',
        toolCallId: 'call-ev-2',
        startedAtMs: Date.now() - 10000,
      };

      render(<EventDrivenNotes initialNotes={[initialNote]} />);
      expect(screen.getByText('Running')).toBeVisible();

      const agentInstance = vi.mocked(HarnxHttpAgent).mock.instances[0] as any;
      act(() => {
        agentInstance.simulateEvent({ type: 'RUN_FINISHED' });
      });
      expect(await screen.findByText('Done')).toBeVisible();
      expect(screen.queryByText('Running')).not.toBeInTheDocument();
    });

    it('transitions Running -> Failed on RUN_ERROR event', async () => {
      const initialNote: SubAgentNote = {
        ...note('running', 'event-child-3'),
        invocationId: 'inv-ev-3',
        toolCallId: 'call-ev-3',
        startedAtMs: Date.now() - 10000,
      };

      render(<EventDrivenNotes initialNotes={[initialNote]} />);
      expect(screen.getByText('Running')).toBeVisible();

      const agentInstance = vi.mocked(HarnxHttpAgent).mock.instances[0] as any;
      act(() => {
        agentInstance.simulateEvent({ type: 'RUN_ERROR' });
      });
      expect(await screen.findByText('Failed')).toBeVisible();
      expect(screen.queryByText('Running')).not.toBeInTheDocument();
    });
  });
});
