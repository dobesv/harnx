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

    expect(state.notes).toEqual([{
      id: 'live:0',
      agent: 'researcher',
      sessionId: 'child-session-0001',
      parentMessageId: 'assistant-parent',
      status: 'running',
    }]);
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
    const state = apply({
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
    });

    expect(state.notes).toEqual([
      {
        id: 'snapshot:assistant-1:call-1',
        agent: 'researcher',
        sessionId: 'reused-child',
        parentMessageId: 'assistant-1',
        status: 'done',
      },
      {
        id: 'snapshot:assistant-2:call-2',
        agent: 'researcher',
        sessionId: 'reused-child',
        parentMessageId: 'assistant-2',
        status: 'done',
      },
    ]);
  });
});
