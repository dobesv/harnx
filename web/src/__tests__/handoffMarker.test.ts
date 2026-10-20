import { describe, it, expect } from 'vitest';
import { parseHandoffMarker } from '../handoffMarker';

describe('parseHandoffMarker', () => {
  const baseMarker = {
    agent: 'pantheon/atlas',
    session_id: 'abc123',
  };

  const basePayload = {
    content: [{ type: 'text', text: 'Handing off to pantheon/atlas; target session: abc123.' }],
    action: 'switch_agent',
    agent: 'pantheon/atlas',
    prompt: 'do work',
    session_id: 'abc123',
    tool_call_id: 'call_1',
    handoff: baseMarker,
  };

  const expectedMarker = {
    agent: 'pantheon/atlas',
    sessionId: 'abc123',
  };

  it.each([
    {
      name: 'parses valid handoff marker from object result',
      inputs: [basePayload],
      expected: expectedMarker,
    },
    {
      name: 'parses valid handoff marker from serialized JSON string result',
      inputs: [JSON.stringify(basePayload)],
      expected: expectedMarker,
    },
    {
      name: 'handles agent names with slashes and colons',
      inputs: [{ handoff: { agent: 'cluster/sub/agent:v1', session_id: 'sess-xyz-789' } }],
      expected: { agent: 'cluster/sub/agent:v1', sessionId: 'sess-xyz-789' },
    },
    {
      name: 'supports camelCase sessionId in handoff marker',
      inputs: [{ handoff: { agent: 'pantheon/atlas', sessionId: 'abc123' } }],
      expected: expectedMarker,
    },
    {
      name: 'trims whitespace around agent and sessionId',
      inputs: [{ handoff: { agent: '  pantheon/atlas  ', session_id: '  abc123  ' } }],
      expected: expectedMarker,
    },
    {
      name: 'falls back to sessionId when session_id is blank or whitespace',
      inputs: [
        { handoff: { agent: 'pantheon/atlas', sessionId: 'abc123' } },
        { handoff: { agent: 'pantheon/atlas', session_id: '', sessionId: 'abc123' } },
        { handoff: { agent: 'pantheon/atlas', session_id: '   ', sessionId: 'abc123' } },
      ],
      expected: expectedMarker,
    },
    {
      name: 'parses nested handoff marker when wrapped in result property',
      inputs: [{ result: { handoff: baseMarker } }],
      expected: expectedMarker,
    },
    {
      name: 'parses nested handoff marker when wrapped in serialized result string',
      inputs: [{ result: JSON.stringify({ handoff: baseMarker }) }],
      expected: expectedMarker,
    },
  ])('$name', ({ inputs, expected }) => {
    for (const input of inputs) {
      expect(parseHandoffMarker(input)).toEqual(expected);
    }
  });

  it.each([
    {
      name: 'returns null when handoff field is absent',
      inputs: [
        {},
        { action: 'switch_agent', agent: 'pantheon/atlas' },
        { content: [{ type: 'text', text: 'no marker' }] },
        '{"content":[]}',
      ],
    },
    {
      name: 'returns null when handoff is not an object',
      inputs: [null, 'string', 123, true, ['pantheon/atlas', 'abc123']].map(handoff => ({ handoff })),
    },
    {
      name: 'returns null when agent is missing, empty, or whitespace',
      inputs: [
        { session_id: 'abc' },
        { agent: '', session_id: 'abc' },
        { agent: '   ', session_id: 'abc' },
        { agent: 123, session_id: 'abc' },
      ].map(handoff => ({ handoff })),
    },
    {
      name: 'returns null when session_id is missing, empty, or whitespace',
      inputs: [
        { agent: 'atlas' },
        { agent: 'atlas', session_id: '' },
        { agent: 'atlas', session_id: '   ' },
        { agent: 'atlas', session_id: 123 },
      ].map(handoff => ({ handoff })),
    },
    {
      name: 'returns null for null, undefined, primitive, or invalid string inputs',
      inputs: [null, undefined, '', 'plain text', '{ invalid json', 12345, true],
    },
  ])('$name', ({ inputs }) => {
    for (const input of inputs) {
      expect(parseHandoffMarker(input)).toBeNull();
    }
  });
});
