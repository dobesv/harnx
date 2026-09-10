import { describe, it, expect, beforeEach } from 'vitest';
import {
  isHandoffConsumed,
  markHandoffConsumed,
  handleHarnxCustomEvent,
} from '../harnxCustomEvents';

describe('handoff deduplication', () => {
  // Helper to track handoff calls
  let handoffCalls: Array<{ agent: string; sessionId: string }> = [];
  
  const makeCallbacks = (sourceSessionId?: string) => ({
    onStatus: () => {},
    onRunFailed: () => {},
    onUsage: () => {},
    onToolSummary: () => {},
    onHandoff: (agent: string, sessionId: string) => {
      handoffCalls.push({ agent, sessionId });
    },
    isRunActive: true,
    sourceSessionId,
  });

  beforeEach(() => {
    handoffCalls = [];
    // Clear consumed handoffs by re-importing - since module state persists,
    // we use unique IDs per test to avoid interference
  });

  it('marks handoff consumed and prevents re-navigation', () => {
    const sourceSessionId = 'test-session-1';
    const toolCallId = 'call-handoff-123';

    expect(isHandoffConsumed(sourceSessionId, toolCallId)).toBe(false);

    markHandoffConsumed(sourceSessionId, toolCallId);

    expect(isHandoffConsumed(sourceSessionId, toolCallId)).toBe(true);
  });

  it('different sessions can have same tool call id without interference', () => {
    const toolCallId = 'call-shared-id';

    expect(isHandoffConsumed('session-a', toolCallId)).toBe(false);
    expect(isHandoffConsumed('session-b', toolCallId)).toBe(false);

    markHandoffConsumed('session-a', toolCallId);

    expect(isHandoffConsumed('session-a', toolCallId)).toBe(true);
    expect(isHandoffConsumed('session-b', toolCallId)).toBe(false);
  });

  it('same session can have multiple different handoffs', () => {
    const sourceSessionId = 'session-multi';

    expect(isHandoffConsumed(sourceSessionId, 'call-1')).toBe(false);
    expect(isHandoffConsumed(sourceSessionId, 'call-2')).toBe(false);

    markHandoffConsumed(sourceSessionId, 'call-1');

    expect(isHandoffConsumed(sourceSessionId, 'call-1')).toBe(true);
    expect(isHandoffConsumed(sourceSessionId, 'call-2')).toBe(false);

    markHandoffConsumed(sourceSessionId, 'call-2');

    expect(isHandoffConsumed(sourceSessionId, 'call-2')).toBe(true);
  });

  it('skips handoff callback when marker already consumed', () => {
    const sourceSessionId = 'session-dedup';
    const toolCallId = 'call-dedup';

    // First, mark as consumed
    markHandoffConsumed(sourceSessionId, toolCallId);

    // Now try to emit the handoff event
    handleHarnxCustomEvent('session_handoff', {
      agent: 'target-agent',
      session_id: 'target-session',
      handoff_tool_call_id: toolCallId,
    }, makeCallbacks(sourceSessionId));

    // Should not call the handoff callback
    expect(handoffCalls).toHaveLength(0);
  });

  it('fires handoff callback when marker not yet consumed', () => {
    const sourceSessionId = 'session-first';
    const toolCallId = 'call-first';

    handleHarnxCustomEvent('session_handoff', {
      agent: 'target-agent',
      session_id: 'target-session',
      handoff_tool_call_id: toolCallId,
    }, makeCallbacks(sourceSessionId));

    expect(handoffCalls).toHaveLength(1);
    expect(handoffCalls[0]).toEqual({
      agent: 'target-agent',
      sessionId: 'target-session',
    });

    // After firing, should be marked consumed
    expect(isHandoffConsumed(sourceSessionId, toolCallId)).toBe(true);
  });

  it('fires handoff without marker id (legacy compatibility)', () => {
    const sourceSessionId = 'session-legacy';

    handleHarnxCustomEvent('session_handoff', {
      agent: 'legacy-agent',
      session_id: 'legacy-session',
    }, makeCallbacks(sourceSessionId));

    expect(handoffCalls).toHaveLength(1);
    expect(handoffCalls[0]).toEqual({
      agent: 'legacy-agent',
      sessionId: 'legacy-session',
    });
  });

  it('fires handoff when sourceSessionId not provided', () => {
    // When sourceSessionId is undefined, deduplication is skipped
    handleHarnxCustomEvent('session_handoff', {
      agent: 'agent-no-source',
      session_id: 'session-no-source',
      handoff_tool_call_id: 'call-no-source',
    }, makeCallbacks());

    expect(handoffCalls).toHaveLength(1);
  });

  it('works for hydrated events (isRunActive false)', () => {
    const sourceSessionId = 'session-hydrated';
    const toolCallId = 'call-hydrated';

    // Hydrated events arrive with isRunActive: false
    const hydratedCallbacks = {
      ...makeCallbacks(sourceSessionId),
      isRunActive: false,
    };

    handleHarnxCustomEvent('session_handoff', {
      agent: 'hydrated-agent',
      session_id: 'hydrated-session',
      handoff_tool_call_id: toolCallId,
    }, hydratedCallbacks as any);

    expect(handoffCalls).toHaveLength(1);
    expect(isHandoffConsumed(sourceSessionId, toolCallId)).toBe(true);

    // Subsequent live event with same marker should be deduped
    handoffCalls = [];
    handleHarnxCustomEvent('session_handoff', {
      agent: 'hydrated-agent',
      session_id: 'hydrated-session',
      handoff_tool_call_id: toolCallId,
    }, makeCallbacks(sourceSessionId));

    expect(handoffCalls).toHaveLength(0);
  });

  it('live-then-hydrated fires onHandoff exactly once', () => {
    const sourceSessionId = 'session-live-then-hydrated';
    const toolCallId = 'call-live-hydrated';

    // First: live event arrives (isRunActive: true)
    handleHarnxCustomEvent('session_handoff', {
      agent: 'live-agent',
      session_id: 'live-session',
      handoff_tool_call_id: toolCallId,
    }, makeCallbacks(sourceSessionId));

    expect(handoffCalls).toHaveLength(1);
    expect(handoffCalls[0]).toEqual({
      agent: 'live-agent',
      sessionId: 'live-session',
    });
    expect(isHandoffConsumed(sourceSessionId, toolCallId)).toBe(true);

    // Second: hydrated event arrives (isRunActive: false) - same marker
    handoffCalls = [];
    const hydratedCallbacks = {
      ...makeCallbacks(sourceSessionId),
      isRunActive: false,
    };
    handleHarnxCustomEvent('session_handoff', {
      agent: 'live-agent',
      session_id: 'live-session',
      handoff_tool_call_id: toolCallId,
    }, hydratedCallbacks as any);

    // Should NOT fire again - marker already consumed
    expect(handoffCalls).toHaveLength(0);
  });

  it('ignores hydrated replay when marker is missing or blank', () => {
    const sourceSessionId = 'session-replay-no-marker';
    const hydratedCallbacks = {
      ...makeCallbacks(sourceSessionId),
      isRunActive: false,
    };

    // 1. handoff_tool_call_id completely omitted
    handleHarnxCustomEvent('session_handoff', {
      agent: 'target-agent',
      session_id: 'target-session',
    }, hydratedCallbacks as any);
    expect(handoffCalls).toHaveLength(0);

    // 2. handoff_tool_call_id is empty string
    handleHarnxCustomEvent('session_handoff', {
      agent: 'target-agent',
      session_id: 'target-session',
      handoff_tool_call_id: '',
    }, hydratedCallbacks as any);
    expect(handoffCalls).toHaveLength(0);

    // 3. handoff_tool_call_id is blank string
    handleHarnxCustomEvent('session_handoff', {
      agent: 'target-agent',
      session_id: 'target-session',
      handoff_tool_call_id: '   ',
    }, hydratedCallbacks as any);
    expect(handoffCalls).toHaveLength(0);
  });

  it('ignores hydrated replay when sourceSessionId is missing', () => {
    const hydratedCallbacks = {
      ...makeCallbacks(), // sourceSessionId undefined
      isRunActive: false,
    };

    handleHarnxCustomEvent('session_handoff', {
      agent: 'target-agent',
      session_id: 'target-session',
      handoff_tool_call_id: 'tool-call-123',
    }, hydratedCallbacks as any);

    expect(handoffCalls).toHaveLength(0);
  });
});
