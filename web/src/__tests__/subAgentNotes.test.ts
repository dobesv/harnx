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
        toolCalls: [{ id: 'call-1' }, { id: 'call-malformed' }],
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
