import { describe, expect, it } from 'vitest';
import { INITIAL_SUB_AGENT_NOTES_STATE, reduceSubAgentNotes } from '../subAgentNotes';

const approvalRef = {
  invocation_id: 'child-invocation',
  tool_round_seq: 10,
  request_seq: 11,
  tool_call_id: 'child-call',
};
const owner = {
  agent: 'researcher',
  cluster: 'shared',
  session_id: 'child-session',
  tool_name: 'write',
  message: 'Write the file?',
  pending_count: 0,
};
const cases = [
  {
    name: 'legacy approval',
    wire: { ...owner, kind: 'legacy_approval', request: approvalRef },
    expected: { type: 'legacy_approval', request: approvalRef },
  },
  {
    name: 'middleware confirm',
    wire: {
      ...owner, kind: 'interaction', interaction_kind: 'confirm',
      interaction_id: 'confirmation', request_seq: 12,
    },
    expected: { type: 'confirm', interactionId: 'confirmation', requestSeq: 12 },
  },
];

function progress(pending?: unknown) {
  return {
    invocation_id: 'child-prompt',
    tool_call_id: 'parent-call',
    agent: 'researcher',
    session_id: 'child-session',
    status: 'running',
    elapsed_ms: 200,
    usage: { input_tokens: 2, output_tokens: 1, cached_tokens: 0 },
    tool_call_count: 1,
    ...(pending === undefined ? {} : { pending_interaction: pending }),
  };
}

function live(pending?: unknown) {
  const state = reduceSubAgentNotes(INITIAL_SUB_AGENT_NOTES_STATE, {
    type: 'TOOL_CALL_START', toolCallId: 'parent-call', parentMessageId: 'assistant-parent',
  });
  return reduceSubAgentNotes(state, {
    type: 'CUSTOM', name: 'sub_agent_progress', value: progress(pending),
  });
}

function expectedPending(expected: object) {
  return expect.objectContaining({
    agent: 'researcher', cluster: 'shared', sessionId: 'child-session',
    toolName: 'write', message: 'Write the file?', pendingCount: 0,
    supported: true, ...expected,
  });
}

describe('sub_agent_progress pending_interaction compatibility', () => {
  it('keeps old progress without the optional field', () => {
    const state = live();
    expect(state.notes).toHaveLength(1);
    expect(state.notes[0]).toMatchObject({
      invocationId: 'child-prompt', status: 'running', toolCallCount: 1,
    });
    expect(state.notes[0].pendingInteraction).toBeUndefined();
  });

  it.each(cases)('parses $name from live custom progress and clears on the next absent field', ({ wire, expected }) => {
    const state = live(wire);
    expect(state.notes).toHaveLength(1);
    expect(state.notes[0].pendingInteraction).toEqual(expectedPending(expected));
    const cleared = reduceSubAgentNotes(state, {
      type: 'CUSTOM', name: 'sub_agent_progress', value: progress(),
    });
    expect(cleared.notes[0].pendingInteraction).toBeUndefined();
    expect(cleared.notes[0].status).toBe('running');
  });

  it.each(cases)('restores $name from persisted parent tool-result catch-up', ({ wire, expected }) => {
    const state = reduceSubAgentNotes(INITIAL_SUB_AGENT_NOTES_STATE, {
      type: 'MESSAGES_SNAPSHOT',
      messages: [
        {
          id: 'assistant-parent', role: 'assistant',
          toolCalls: [{ id: 'parent-call' }],
        },
        {
          role: 'tool', toolCallId: 'parent-call',
          content: JSON.stringify({
            status: 'blocked', terminal: false,
            sub_agent: { agent: 'researcher', session_id: 'child-session' },
            sub_agent_progress: progress(wire),
          }),
        },
      ],
    });
    expect(state.notes).toHaveLength(1);
    expect(state.notes[0].pendingInteraction).toEqual(expectedPending(expected));
    expect(state.notes[0].status).toBe('running');
  });

  it('does not discard valid progress because its optional summary is malformed', () => {
    const state = live({ kind: 'legacy_approval', request: {} });
    expect(state.notes).toHaveLength(1);
    expect(state.notes[0].pendingInteraction).toBeUndefined();
    expect(state.notes[0].invocationId).toBe('child-prompt');
  });
});
