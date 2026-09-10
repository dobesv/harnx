import { describe, expect, it } from 'vitest';
import {
  INITIAL_SUB_AGENT_NOTES_STATE,
  reduceSubAgentNotes,
  type SubAgentNotesState,
} from '../subAgentNotes';

function apply(...events: unknown[]): SubAgentNotesState {
  return events.reduce(reduceSubAgentNotes, INITIAL_SUB_AGENT_NOTES_STATE);
}

const toolStart = (parentMessageId = 'assistant-parent') => ({
  type: 'TOOL_CALL_START',
  toolCallId: 'call-1',
  parentMessageId,
});

const started = (agent: unknown, sessionId: unknown) => ({
  type: 'CUSTOM',
  name: 'sub_agent_started',
  value: { agent, session_id: sessionId },
});

const completed = (agent: string, sessionId: string) => ({
  type: 'TOOL_CALL_RESULT',
  content: JSON.stringify({ sub_agent: { agent, session_id: sessionId } }),
});

const progress = (
  invocationId: string,
  status: 'running' | 'done' | 'failed',
  elapsedMs: number,
) => ({
  type: 'CUSTOM',
  name: 'sub_agent_progress',
  value: {
    invocation_id: invocationId,
    agent: 'researcher',
    session_id: 'child-session-0001',
    status,
    elapsed_ms: elapsedMs,
    usage: { input_tokens: 120, output_tokens: 45, cached_tokens: 30 },
    tool_call_count: 3,
  },
});

function snapshotEvent() {
  return {
    type: 'MESSAGES_SNAPSHOT',
    messages: [
      {
        id: 'assistant-1',
        role: 'assistant',
        toolCalls: [{ id: 'call-1' }, { id: 'call-malformed' }, { id: 'call-3-missing-result' }],
      },
      {
        role: 'tool',
        toolCallId: 'call-1',
        content: JSON.stringify({
          sub_agent: { agent: 'researcher', session_id: 'reused-child' },
          sub_agent_progress: {
            invocation_id: 'snapshot-inv-1',
            agent: 'researcher',
            session_id: 'reused-child',
            status: 'done',
            elapsed_ms: 12_345,
            usage: { input_tokens: 120, output_tokens: 45, cached_tokens: 30 },
            tool_call_count: 3,
          },
        }),
      },
      {
        role: 'tool',
        toolCallId: 'call-malformed',
        content: JSON.stringify({
          sub_agent: { agent: 'researcher', session_id: '' },
        }),
      },
      {
        id: 'assistant-2',
        role: 'assistant',
        tool_calls: [{ id: 'call-2' }],
      },
      {
        role: 'tool',
        tool_call_id: 'call-2',
        content: JSON.stringify({
          sub_agent: { agent: 'researcher', session_id: 'reused-child' },
        }),
      },
    ],
  };
}

describe('reduceSubAgentNotes', () => {
  it('adds valid starts and ignores malformed identities or starts without a tool parent', () => {
    const state = apply(
      started('researcher', 'orphan'),
      toolStart(),
      started('', 'session-1'),
      started('researcher', '   '),
      started(null, 'session-1'),
      started('researcher', 'child-session-0001'),
    );

    expect(state.notes).toEqual([expect.objectContaining({
      id: 'live:0',
      agent: 'researcher',
      sessionId: 'child-session-0001',
      parentMessageId: 'assistant-parent',
      status: 'running',
      elapsedMs: 0,
      inputTokens: 0,
      outputTokens: 0,
      cachedTokens: 0,
      toolCallCount: 0,
    })]);
  });

  it('completes only the latest running note with a matching structured marker', () => {
    const running = apply(
      toolStart(),
      started('researcher', 'child-session-0001'),
    );
    const malformed = reduceSubAgentNotes(running, {
      type: 'TOOL_CALL_RESULT',
      content: '{not json',
    });
    const mismatch = reduceSubAgentNotes(malformed, completed('researcher', 'another-session'));
    const done = reduceSubAgentNotes(mismatch, completed('researcher', 'child-session-0001'));

    expect(malformed).toBe(running);
    expect(mismatch).toBe(running);
    expect(done.notes[0].status).toBe('done');
  });

  it('marks unresolved rows failed when the parent run ends', () => {
    const state = apply(
      toolStart('assistant-1'),
      started('researcher', 'finished-child'),
      completed('researcher', 'finished-child'),
      toolStart('assistant-2'),
      started('reviewer', 'unfinished-child'),
      { type: 'RUN_ERROR' },
    );

    expect(state.notes.map((note) => note.status)).toEqual(['done', 'failed']);
  });

  it('correlates live metrics and terminal state by invocation id', () => {
    const state = apply(
      toolStart(),
      {
        type: 'CUSTOM',
        name: 'sub_agent_started',
        value: {
          agent: 'researcher',
          session_id: 'child-session-0001',
          invocation_id: 'inv-1',
        },
      },
      progress('inv-1', 'running', 10_000),
      progress('inv-1', 'done', 12_345),
    );

    expect(state.notes).toEqual([expect.objectContaining({
      id: 'live:inv-1',
      invocationId: 'inv-1',
      status: 'done',
      elapsedMs: 12_345,
      inputTokens: 120,
      outputTokens: 45,
      cachedTokens: 30,
      toolCallCount: 3,
    })]);
  });

  it('does not reopen a terminal invocation after late running events', () => {
    const start = {
      type: 'CUSTOM',
      name: 'sub_agent_started',
      value: {
        agent: 'researcher',
        session_id: 'child-session-0001',
        invocation_id: 'inv-1',
      },
    };
    const state = apply(
      toolStart(),
      start,
      progress('inv-1', 'done', 12_345),
      progress('inv-1', 'running', 20_000),
      start,
    );

    expect(state.notes).toEqual([expect.objectContaining({
      invocationId: 'inv-1',
      status: 'done',
      elapsedMs: 12_345,
    })]);
  });

  it('keeps concurrent invocations of a reused child session distinct', () => {
    const state = apply(
      toolStart('assistant-1'),
      {
        ...started('researcher', 'child-session-0001'),
        value: {
          agent: 'researcher',
          session_id: 'child-session-0001',
          invocation_id: 'inv-1',
        },
      },
      toolStart('assistant-2'),
      {
        ...started('researcher', 'child-session-0001'),
        value: {
          agent: 'researcher',
          session_id: 'child-session-0001',
          invocation_id: 'inv-2',
        },
      },
      progress('inv-1', 'done', 1_000),
      progress('inv-2', 'running', 2_000),
    );

    expect(state.notes.map((note) => note.invocationId)).toEqual(['inv-1', 'inv-2']);
    expect(state.notes.map((note) => note.parentMessageId)).toEqual([
      'assistant-1',
      'assistant-2',
    ]);
  });

  it('is idempotent for duplicate delivery but records later reuse of one child session', () => {
    const state = apply(
      toolStart('assistant-1'),
      started('researcher', 'reused-child'),
      started('researcher', 'reused-child'),
      completed('researcher', 'reused-child'),
      completed('researcher', 'reused-child'),
      toolStart('assistant-2'),
      started('researcher', 'reused-child'),
      completed('researcher', 'reused-child'),
    );

    expect(state.notes).toHaveLength(2);
    expect(state.notes.map((note) => ({
      parentMessageId: note.parentMessageId,
      status: note.status,
    }))).toEqual([
      { parentMessageId: 'assistant-1', status: 'done' },
      { parentMessageId: 'assistant-2', status: 'done' },
    ]);
  });

  it('seeds rows from hydrated sub_agent_started with matching tool-result via snapshot', () => {
    const state = apply(
      snapshotEvent(),
      {
        type: 'CUSTOM',
        name: 'sub_agent_started',
        value: {
          agent: 'researcher',
          session_id: 'reused-child',
          invocation_id: 'snapshot-inv-1',
          tool_call_id: 'call-1',
          started_at: '2026-09-09T05:15:30Z'
        }
      }
    );

    expect(state.notes.length).toBe(2);
    // Should NOT duplicate the completed row, because toolCallId matches and it's already "done"
    expect(state.notes[0]).toMatchObject({
      id: 'snapshot:assistant-1:call-1',
      status: 'done',
      agent: 'researcher',
      toolCallId: 'call-1',
    });
  });

  it('seeds running rows from hydrated sub_agent_started without matching tool-result', () => {
    const state = apply(
      snapshotEvent(),
      {
        type: 'CUSTOM',
        name: 'sub_agent_started',
        value: {
          agent: 'coder',
          session_id: 'child-coder-1',
          invocation_id: 'inv-coder-1',
          tool_call_id: 'call-3-missing-result',
          started_at: '2026-09-09T05:15:30Z'
        }
      }
    );

    expect(state.notes.length).toBe(3); // 2 from snapshot + 1 running
    expect(state.notes[2]).toMatchObject({
      id: 'live:inv-coder-1',
      status: 'running',
      agent: 'coder',
      sessionId: 'child-coder-1',
      toolCallId: 'call-3-missing-result',
      startedAtMs: new Date('2026-09-09T05:15:30Z').getTime()
    });
  });

  it('classifies CHILD_TERMINAL to fail a running note', () => {
    const state = apply(
      snapshotEvent(),
      {
        type: 'CUSTOM',
        name: 'sub_agent_started',
        value: {
          agent: 'coder',
          session_id: 'child-coder-1',
          invocation_id: 'inv-coder-1',
          tool_call_id: 'call-3-missing-result',
          started_at: '2026-09-09T05:15:30Z'
        }
      },
      {
        type: 'CHILD_TERMINAL',
        invocationId: 'inv-coder-1',
        status: 'failed'
      }
    );

    expect(state.notes[2]).toMatchObject({
      id: 'live:inv-coder-1',
      status: 'failed',
    });
  });
  it('allows parent authoritative result to override a child-liveness failed state', () => {
    const state = apply(
      snapshotEvent(),
      {
        type: 'CUSTOM',
        name: 'sub_agent_started',
        value: {
          agent: 'coder',
          session_id: 'child-coder-1',
          invocation_id: 'inv-coder-1',
          tool_call_id: 'call-3-missing-result',
          started_at: '2026-09-09T05:15:30Z'
        }
      },
      {
        type: 'CHILD_TERMINAL',
        invocationId: 'inv-coder-1',
        status: 'failed'
      },
      {
        type: 'TOOL_CALL_RESULT',
        toolCallId: 'call-3-missing-result',
        content: JSON.stringify({
          sub_agent_progress: {
            invocation_id: 'inv-coder-1',
            agent: 'coder',
            session_id: 'child-coder-1',
            status: 'done',
            elapsed_ms: 1000,
            usage: { input_tokens: 10, output_tokens: 20, cached_tokens: 0 },
            tool_call_count: 1
          }
        })
      }
    );

    expect(state.notes[2]).toMatchObject({
      id: 'live:inv-coder-1',
      status: 'done',
      elapsedMs: 1000,
      inputTokens: 10,
      outputTokens: 20
    });
  });

  it('allows parent authoritative result marker to mark a child-liveness failed state as done', () => {
    const state = apply(
      snapshotEvent(),
      {
        type: 'CUSTOM',
        name: 'sub_agent_started',
        value: {
          agent: 'coder',
          session_id: 'child-coder-1',
          invocation_id: 'inv-coder-1',
          tool_call_id: 'call-3-missing-result',
          started_at: '2026-09-09T05:15:30Z'
        }
      },
      {
        type: 'CHILD_TERMINAL',
        invocationId: 'inv-coder-1',
        status: 'failed'
      },
      {
        type: 'TOOL_CALL_RESULT',
        toolCallId: 'call-3-missing-result',
        content: JSON.stringify({
          sub_agent: {
            agent: 'coder',
            session_id: 'child-coder-1',
            invocation_id: 'inv-coder-1'
          }
        })
      }
    );

    expect(state.notes[2]).toMatchObject({
      id: 'live:inv-coder-1',
      status: 'done'
    });
  });

  it('freezes elapsedMs on CHILD_TERMINAL by accumulating localElapsed', () => {
    const startTime = Date.now() - 5000;
    const initial = apply(
      toolStart('parent-msg'),
      {
        type: 'CUSTOM',
        name: 'sub_agent_started',
        value: {
          agent: 'coder',
          session_id: 'child-1',
          invocation_id: 'inv-freeze-1',
          tool_call_id: 'call-1',
          started_at: new Date(startTime).toISOString(),
        },
      }
    );

    const runningNote = initial.notes[0];
    const modifiedState: SubAgentNotesState = {
      ...initial,
      notes: [{
        ...runningNote,
        elapsedMs: 1500,
        updatedAtMs: Date.now() - 2000,
      }],
    };

    const terminalState = reduceSubAgentNotes(modifiedState, {
      type: 'CHILD_TERMINAL',
      invocationId: 'inv-freeze-1',
      status: 'done',
    });

    const frozenNote = terminalState.notes[0];
    expect(frozenNote.status).toBe('done');
    expect(frozenNote.elapsedMs).toBeGreaterThanOrEqual(3400);
  });

  it('preserves startedAtMs and toolCallId when applyProgress receives update without started_at', () => {
    const startTime = 1725800000000;
    const initial = apply(
      toolStart('parent-msg'),
      {
        type: 'CUSTOM',
        name: 'sub_agent_started',
        value: {
          agent: 'coder',
          session_id: 'child-1',
          invocation_id: 'inv-prog-1',
          tool_call_id: 'call-1',
          started_at: new Date(startTime).toISOString(),
        },
      }
    );

    expect(initial.notes[0].startedAtMs).toBe(startTime);
    expect(initial.notes[0].toolCallId).toBe('call-1');

    const updated = reduceSubAgentNotes(initial, {
      type: 'CUSTOM',
      name: 'sub_agent_progress',
      value: {
        agent: 'coder',
        session_id: 'child-1',
        invocation_id: 'inv-prog-1',
        status: 'running',
        elapsed_ms: 2500,
        usage: { input_tokens: 50, output_tokens: 25, cached_tokens: 10 },
        tool_call_count: 2,
      },
    });

    expect(updated.notes[0].startedAtMs).toBe(startTime);
    expect(updated.notes[0].toolCallId).toBe('call-1');
    expect(updated.notes[0].elapsedMs).toBe(2500);
  });

  it('restores completed rows under their launching assistant messages from a snapshot', () => {
    const state = apply(snapshotEvent());

    expect(state.notes).toEqual([
      expect.objectContaining({
        id: 'snapshot:assistant-1:call-1',
        invocationId: 'snapshot-inv-1',
        agent: 'researcher',
        sessionId: 'reused-child',
        parentMessageId: 'assistant-1',
        status: 'done',
        elapsedMs: 12_345,
        inputTokens: 120,
        outputTokens: 45,
        cachedTokens: 30,
        toolCallCount: 3,
      }),
      expect.objectContaining({
        id: 'snapshot:assistant-2:call-2',
        agent: 'researcher',
        sessionId: 'reused-child',
        parentMessageId: 'assistant-2',
        status: 'done',
        elapsedMs: 0,
      }),
    ]);
  });
});
