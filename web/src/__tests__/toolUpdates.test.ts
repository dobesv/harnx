import { describe, it, expect } from 'vitest';
import {
  initToolCallState,
  applyToolUpdate,
  reduceToolUpdates,
  parseToolUpdateEvent,
  type ToolCallState,
  type ToolCallUpdatePatch,
  type ToolUpdatesState,
} from '../toolUpdates';

const TOOL_CALL_ID = 'tc-123';

type UpdateFields = Omit<ToolCallUpdatePatch, 'tool_call_id'>;

function makeUpdatePatch(
  fields: UpdateFields = {},
  toolCallId = TOOL_CALL_ID,
): ToolCallUpdatePatch {
  return { tool_call_id: toolCallId, ...fields };
}

function makeToolCallState(overrides: Partial<ToolCallState> = {}): ToolCallState {
  return { tool_call_id: TOOL_CALL_ID, locations: [], ...overrides };
}

function applyUpdate(
  state: ToolCallState,
  fields: UpdateFields,
): ToolCallState {
  return applyToolUpdate(state, makeUpdatePatch(fields, state.tool_call_id));
}

function reduceUpdate(
  state: ToolUpdatesState,
  fields: UpdateFields,
  toolCallId = TOOL_CALL_ID,
): ToolUpdatesState {
  return reduceToolUpdates(state, {
    type: 'TOOL_UPDATE',
    patch: makeUpdatePatch(fields, toolCallId),
  });
}

function makeUpdateEvent(
  fields: Record<string, unknown> = {},
  toolCallId = TOOL_CALL_ID,
): Record<string, unknown> {
  return { tool_call_id: toolCallId, ...fields };
}

describe('toolUpdates', () => {
  describe('initToolCallState', () => {
    it('initializes with id and empty locations', () => {
      const state = initToolCallState('tc-123');
      expect(state.tool_call_id).toBe('tc-123');
      expect(state.locations).toEqual([]);
      expect(state.markdown).toBeUndefined();
      expect(state.title).toBeUndefined();
      expect(state.kind).toBeUndefined();
      expect(state.status).toBeUndefined();
      expect(state.usage).toBeUndefined();
    });
  });

  describe('applyToolUpdate', () => {
    it.each([
      ['markdown', { markdown: 'new summary' }, 'new summary'],
      ['title', { title: 'Processing...' }, 'Processing...'],
      ['status', { status: 'InProgress' as const }, 'InProgress'],
    ])('applies %s patch', (field, patch, expected) => {
      const updated = applyUpdate(initToolCallState(TOOL_CALL_ID), patch);
      expect(updated[field as keyof ToolCallState]).toBe(expected);
    });

    it('replaces locations', () => {
      const state = makeToolCallState({ locations: [{ path: 'old.rs' }] });
      const updated = applyUpdate(state, {
        locations: [{ path: 'new.rs', line: 10 }],
      });
      expect(updated.locations).toEqual([{ path: 'new.rs', line: 10 }]);
    });

    it('clears locations with empty array', () => {
      const state = makeToolCallState({ locations: [{ path: 'old.rs' }] });
      const updated = applyUpdate(state, { locations: [] });
      expect(updated.locations).toEqual([]);
    });

    it('leaves locations unchanged when undefined', () => {
      const state = makeToolCallState({ locations: [{ path: 'keep.rs' }] });
      const updated = applyUpdate(state, { title: 'New title' });
      expect(updated.locations).toEqual([{ path: 'keep.rs' }]);
    });

    it('applies usage patch', () => {
      const usage = { input_tokens: 100, output_tokens: 50, cached_tokens: 10 };
      const updated = applyUpdate(initToolCallState(TOOL_CALL_ID), { usage });
      expect(updated.usage).toEqual(usage);
    });

    it('preserves existing fields when not patched', () => {
      const state = makeToolCallState({
        markdown: 'existing',
        title: 'existing title',
        status: 'InProgress',
        locations: [{ path: 'file.rs' }],
      });
      const updated = applyUpdate(state, { title: 'updated title' });
      expect(updated.markdown).toBe('existing');
      expect(updated.status).toBe('InProgress');
      expect(updated.locations).toEqual([{ path: 'file.rs' }]);
      expect(updated.title).toBe('updated title');
    });
  });

  describe('reduceToolUpdates', () => {
    it('starts with empty state', () => {
      expect(new Map().size).toBe(0);
    });

    it('handles TOOL_UPDATE creating new entry', () => {
      const newState = reduceUpdate(new Map(), { title: 'New tool' });
      expect(newState.has(TOOL_CALL_ID)).toBe(true);
      expect(newState.get(TOOL_CALL_ID)?.title).toBe('New tool');
    });

    it('handles TOOL_UPDATE updating existing entry', () => {
      const state = new Map([[TOOL_CALL_ID, makeToolCallState({ title: 'Old' })]]);
      const newState = reduceUpdate(state, { title: 'Updated' });
      expect(newState.get(TOOL_CALL_ID)?.title).toBe('Updated');
    });

    it('handles RESET clearing all entries', () => {
      const state = new Map([
        [TOOL_CALL_ID, makeToolCallState()],
        ['tc-456', makeToolCallState({ tool_call_id: 'tc-456' })],
      ]);
      const newState = reduceToolUpdates(state, { type: 'RESET' });
      expect(newState.size).toBe(0);
    });

    it('does not duplicate entries on multiple updates', () => {
      let newState = reduceUpdate(new Map(), { title: 'First' });
      newState = reduceUpdate(newState, { title: 'Second' });
      newState = reduceUpdate(newState, { status: 'InProgress' });
      expect(newState.size).toBe(1);
      expect(newState.get(TOOL_CALL_ID)?.title).toBe('Second');
      expect(newState.get(TOOL_CALL_ID)?.status).toBe('InProgress');
    });
  });

  describe('parseToolUpdateEvent', () => {
    it('parses complete payload', () => {
      const usage = { input_tokens: 100, output_tokens: 50, cached_tokens: 10 };
      const locations = [{ path: 'src/main.rs', line: 42 }];
      const patch = parseToolUpdateEvent(makeUpdateEvent({
        markdown: 'summary',
        status: 'InProgress',
        title: 'Processing',
        kind: 'Read',
        locations,
        usage,
      }));
      expect(patch).toMatchObject({
        tool_call_id: TOOL_CALL_ID,
        markdown: 'summary',
        status: 'InProgress',
        title: 'Processing',
        kind: 'Read',
        locations,
        usage,
      });
    });

    it('parses partial payload', () => {
      const patch = parseToolUpdateEvent(makeUpdateEvent(
        { title: 'Only title' },
        'tc-456',
      ));
      expect(patch).toMatchObject({
        tool_call_id: 'tc-456',
        title: 'Only title',
      });
      expect(patch?.markdown).toBeUndefined();
      expect(patch?.locations).toBeUndefined();
    });

    it('parses empty locations array', () => {
      const patch = parseToolUpdateEvent(makeUpdateEvent({ locations: [] }));
      expect(patch?.locations).toEqual([]);
    });

    it.each([
      ['missing tool_call_id', { title: 'no id' }],
      ['null input', null],
      ['non-object input', 'string'],
    ])('returns undefined for %s', (_scenario, input) => {
      expect(parseToolUpdateEvent(input)).toBeUndefined();
    });

    it('filters invalid locations', () => {
      const patch = parseToolUpdateEvent(makeUpdateEvent({
        locations: [
          { path: 'valid.rs', line: 1 },
          { line: 2 },
          { path: 123, line: 3 },
          null,
          { path: 'another.rs' },
        ],
      }));
      expect(patch?.locations).toEqual([
        { path: 'valid.rs', line: 1 },
        { path: 'another.rs', line: undefined },
      ]);
    });

    it.each([
      ['status', { status: 'InvalidStatus' }],
      ['kind', { kind: 'InvalidKind' }],
    ])('ignores invalid %s values', (field, event) => {
      const patch = parseToolUpdateEvent(makeUpdateEvent(event));
      expect(patch?.[field as 'status' | 'kind']).toBeUndefined();
    });
  });
});
