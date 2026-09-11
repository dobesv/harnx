import { describe, it, expect, vi } from 'vitest';
import { HarnxHttpAgent } from '../ChatProvider';
import { handleHarnxCustomEvent, type HarnxCustomEventCallbacks } from '../harnxCustomEvents';

function createAgent(onHandoff = vi.fn()) {
  const agent = new HarnxHttpAgent({
    url: '/test-agent',
    onStatus: vi.fn(),
    onRunFailed: vi.fn(),
    onUsage: vi.fn(),
    onToolSummary: vi.fn(),
    onHandoff,
    onSubAgentEvent: vi.fn(),
  });

  let capturedSubscriber: any;
  vi.spyOn(Object.getPrototypeOf(Object.getPrototypeOf(agent)), 'runAgent').mockImplementation(
    (_params: any, subscriber: any) => {
      capturedSubscriber = subscriber;
      return Promise.resolve();
    }
  );
  agent.runAgent({});

  return {
    agent,
    onHandoff,
    dispatch: (event: { type: string; name?: string; value?: unknown }) =>
      capturedSubscriber.onEvent({ event }),
  };
}

describe('handoff attach-seq gating', () => {
  it('gates handoffs purely by sequence relative to boundary', async () => {
    const { dispatch, onHandoff } = createAgent();

    await dispatch({
      type: 'CUSTOM',
      name: 'session_attach_boundary',
      value: { attached_seq: 7 },
    });

    // seq == boundary => no nav
    await dispatch({
      type: 'CUSTOM',
      name: 'session_handoff',
      value: { agent: 'target', session_id: 's1', after_seq: 7 },
    });
    expect(onHandoff).not.toHaveBeenCalled();

    // seq < boundary => no nav
    await dispatch({
      type: 'CUSTOM',
      name: 'session_handoff',
      value: { agent: 'target', session_id: 's1', after_seq: 6 },
    });
    expect(onHandoff).not.toHaveBeenCalled();

    // seq > boundary => nav once
    await dispatch({
      type: 'CUSTOM',
      name: 'session_handoff',
      value: { agent: 'target', session_id: 's1', after_seq: 8 },
    });
    expect(onHandoff).toHaveBeenCalledTimes(1);
    expect(onHandoff).toHaveBeenCalledWith('target', 's1');

    // deliver 8 again => still no additional nav
    await dispatch({
      type: 'CUSTOM',
      name: 'session_handoff',
      value: { agent: 'target', session_id: 's1', after_seq: 8 },
    });
    expect(onHandoff).toHaveBeenCalledTimes(1);

    // then seq=9 => nav again
    await dispatch({
      type: 'CUSTOM',
      name: 'session_handoff',
      value: { agent: 'target2', session_id: 's2', after_seq: 9 },
    });
    expect(onHandoff).toHaveBeenCalledTimes(2);
    expect(onHandoff).toHaveBeenLastCalledWith('target2', 's2');
  });

  it('fails closed when handoff arrives before any session_attach_boundary', async () => {
    const { dispatch, onHandoff } = createAgent();

    await dispatch({
      type: 'CUSTOM',
      name: 'session_handoff',
      value: { agent: 'target', session_id: 's1', after_seq: 10 },
    });
    expect(onHandoff).not.toHaveBeenCalled();
  });

  it('fails closed on invalid afterSeq values', async () => {
    const { dispatch, onHandoff } = createAgent();

    await dispatch({
      type: 'CUSTOM',
      name: 'session_attach_boundary',
      value: { attached_seq: 5 },
    });

    const invalidValues = [
      undefined,
      null,
      -1,
      -10,
      6.5,
      Number.MAX_SAFE_INTEGER + 100,
      '8',
      NaN,
      Infinity,
      -Infinity,
      {},
      [],
    ];

    for (const invalidSeq of invalidValues) {
      await dispatch({
        type: 'CUSTOM',
        name: 'session_handoff',
        value: { agent: 'target', session_id: 's1', after_seq: invalidSeq },
      });
      expect(onHandoff).not.toHaveBeenCalled();
    }
  });

  it('prevents re-navigation on reload (issue #1803)', async () => {
    const onHandoffA = vi.fn();
    const clientA = createAgent(onHandoffA);

    await clientA.dispatch({
      type: 'CUSTOM',
      name: 'session_attach_boundary',
      value: { attached_seq: 7 },
    });

    // Live handoff arrives
    await clientA.dispatch({
      type: 'CUSTOM',
      name: 'session_handoff',
      value: { agent: 'target', session_id: 'target-session', after_seq: 8 },
    });
    expect(onHandoffA).toHaveBeenCalledWith('target', 'target-session');

    // Reload creates a fresh agent instance for the source session view
    const onHandoffB = vi.fn();
    const clientB = createAgent(onHandoffB);

    // After commit, durable tail is at least 8 (here attach boundary is 9)
    await clientB.dispatch({
      type: 'CUSTOM',
      name: 'session_attach_boundary',
      value: { attached_seq: 9 },
    });

    // Replayed handoff arrives from history
    await clientB.dispatch({
      type: 'CUSTOM',
      name: 'session_handoff',
      value: { agent: 'target', session_id: 'target-session', after_seq: 8 },
    });
    expect(onHandoffB).not.toHaveBeenCalled();
  });

  it('prevents navigation for second client attaching to already-handed-off session', async () => {
    const { dispatch, onHandoff } = createAgent();

    // Client attaches when durable tail is 8 (the handoff has already been committed)
    await dispatch({
      type: 'CUSTOM',
      name: 'session_attach_boundary',
      value: { attached_seq: 8 },
    });

    // Replayed handoff with after_seq 8
    await dispatch({
      type: 'CUSTOM',
      name: 'session_handoff',
      value: { agent: 'target', session_id: 'target-session', after_seq: 8 },
    });
    expect(onHandoff).not.toHaveBeenCalled();
  });

  it('gates markerless handoff purely on seq', async () => {
    const { dispatch, onHandoff } = createAgent();

    await dispatch({
      type: 'CUSTOM',
      name: 'session_attach_boundary',
      value: { attached_seq: 10 },
    });

    // Markerless handoff with seq <= boundary => no nav
    await dispatch({
      type: 'CUSTOM',
      name: 'session_handoff',
      value: { agent: 'target', session_id: 'target-session', after_seq: 10 },
    });
    expect(onHandoff).not.toHaveBeenCalled();

    // Markerless handoff with seq > boundary => navigates
    await dispatch({
      type: 'CUSTOM',
      name: 'session_handoff',
      value: { agent: 'target', session_id: 'target-session', after_seq: 11 },
    });
    expect(onHandoff).toHaveBeenCalledWith('target', 'target-session');
  });

  describe('handleHarnxCustomEvent handler units', () => {
    const makeCallbacks = (overrides?: Partial<HarnxCustomEventCallbacks>): HarnxCustomEventCallbacks => ({
      onStatus: vi.fn(),
      onRunFailed: vi.fn(),
      onUsage: vi.fn(),
      onToolSummary: vi.fn(),
      ...overrides,
    });

    it('dispatches session_attach_boundary when attached_seq is valid', () => {
      const onAttachBoundary = vi.fn();
      const callbacks = makeCallbacks({ onAttachBoundary });

      handleHarnxCustomEvent('session_attach_boundary', { attached_seq: 42 }, callbacks);
      expect(onAttachBoundary).toHaveBeenCalledWith(42);

      handleHarnxCustomEvent('session_attach_boundary', { attached_seq: 0 }, callbacks);
      expect(onAttachBoundary).toHaveBeenCalledWith(0);
    });

    it('ignores session_attach_boundary when attached_seq is invalid or missing', () => {
      const onAttachBoundary = vi.fn();
      const callbacks = makeCallbacks({ onAttachBoundary });

      handleHarnxCustomEvent('session_attach_boundary', {}, callbacks);
      handleHarnxCustomEvent('session_attach_boundary', { attached_seq: -1 }, callbacks);
      handleHarnxCustomEvent('session_attach_boundary', { attached_seq: 1.5 }, callbacks);
      handleHarnxCustomEvent('session_attach_boundary', { attached_seq: '42' }, callbacks);
      handleHarnxCustomEvent('session_attach_boundary', { attached_seq: null }, callbacks);
      expect(onAttachBoundary).not.toHaveBeenCalled();
    });

    it('dispatches session_handoff when after_seq is valid', () => {
      const onHandoff = vi.fn();
      const callbacks = makeCallbacks({ onHandoff });

      handleHarnxCustomEvent(
        'session_handoff',
        { agent: 'agent1', session_id: 'sess1', after_seq: 15 },
        callbacks
      );
      expect(onHandoff).toHaveBeenCalledWith('agent1', 'sess1', 15);
    });

    it('ignores session_handoff when after_seq or target is invalid', () => {
      const onHandoff = vi.fn();
      const callbacks = makeCallbacks({ onHandoff });

      // Missing after_seq
      handleHarnxCustomEvent(
        'session_handoff',
        { agent: 'agent1', session_id: 'sess1' },
        callbacks
      );
      // Negative after_seq
      handleHarnxCustomEvent(
        'session_handoff',
        { agent: 'agent1', session_id: 'sess1', after_seq: -1 },
        callbacks
      );
      // Blank agent
      handleHarnxCustomEvent(
        'session_handoff',
        { agent: '   ', session_id: 'sess1', after_seq: 5 },
        callbacks
      );
      // Blank session_id
      handleHarnxCustomEvent(
        'session_handoff',
        { agent: 'agent1', session_id: '', after_seq: 5 },
        callbacks
      );
      expect(onHandoff).not.toHaveBeenCalled();
    });
  });
});
