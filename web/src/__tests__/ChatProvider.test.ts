import { describe, it, expect, vi } from 'vitest';
import { attachmentToMessageParts, toAgUiMessages } from '../ChatProvider';
import type { Message } from '@ag-ui/client';

async function dispatchAgentEvent(subscriber: any, event: any) {
  await subscriber.onEvent({ event });
}

describe('attachmentToMessageParts', () => {
  it('should return image part', () => {
    const attachment = {
      name: 'test.png',
      content: [{ type: 'image', image: 'cid:img', filename: 'test.png' }],
    };
    expect(attachmentToMessageParts(attachment)).toEqual([
      { type: 'image', image: 'cid:img', filename: 'test.png' },
    ]);
  });

  it('should return file part', () => {
    const attachment = {
      name: 'test.pdf',
      content: [{ type: 'file', data: 'cid:file', mimeType: 'application/pdf', filename: 'test.pdf' }],
    };
    expect(attachmentToMessageParts(attachment)).toEqual([
      { type: 'file', data: 'cid:file', mimeType: 'application/pdf', filename: 'test.pdf' },
    ]);
  });

  it('should return empty array if no content', () => {
    const attachment = {};
    expect(attachmentToMessageParts(attachment)).toEqual([]);
  });

  it('should return multiple parts for multipart attachment', () => {
    // No attachment-level `name`, so each part keeps its own filename.
    const attachment = {
      content: [
        { type: 'image', image: 'cid:img', filename: 'multi.png' },
        { type: 'file', data: 'cid:file', mimeType: 'text/plain', filename: 'multi.txt' }
      ],
    };
    expect(attachmentToMessageParts(attachment)).toEqual([
      { type: 'image', image: 'cid:img', filename: 'multi.png' },
      { type: 'file', data: 'cid:file', mimeType: 'text/plain', filename: 'multi.txt' }
    ]);
  });

  it('should prefer attachment-level name over per-part filename', () => {
    const attachment = {
      name: 'override',
      content: [{ type: 'image', image: 'cid:img', filename: 'ignored.png' }],
    };
    expect(attachmentToMessageParts(attachment)).toEqual([
      { type: 'image', image: 'cid:img', filename: 'override' },
    ]);
  });
});

describe('toAgUiMessages', () => {
  it('should preserve role and ignore activity messages', () => {
    const messages: Message[] = [
      { role: 'activity', content: 'skip' } as any,
      { role: 'assistant', content: 'hello' },
    ];
    expect(toAgUiMessages(messages)).toEqual([{ role: 'assistant', content: 'hello' }]);
  });

  it('should handle string content WITH attachments (regression test)', () => {
    const messages: any[] = [
      {
        role: 'user',
        content: 'hello',
        attachments: [
          { content: [{ type: 'image', image: 'cid:img', filename: 'test.png' }] },
        ],
      },
    ];
    const result = toAgUiMessages(messages);
    expect(result[0].content).toEqual([
      { type: 'text', text: 'hello' },
      { type: 'image', image: 'cid:img', filename: 'test.png' },
    ]);
  });

  it('should handle array content with attachments', () => {
    const messages: any[] = [
      {
        role: 'user',
        content: [{ type: 'text', text: 'hello' }],
        attachments: [
          { content: [{ type: 'image', image: 'cid:img', filename: 'test.png' }] },
        ],
      },
    ];
    const result = toAgUiMessages(messages);
    expect(result[0].content).toEqual([
      { type: 'text', text: 'hello' },
      { type: 'image', image: 'cid:img', filename: 'test.png' },
    ]);
  });

  it('should not drop attachments on null content (regression test)', () => {
    const messages: any[] = [
      {
        role: 'user',
        content: null,
        attachments: [
          { content: [{ type: 'image', image: 'cid:img', filename: 'test.png' }] },
        ],
      },
    ];
    const result = toAgUiMessages(messages);
    expect(result[0].content).toEqual([
      { type: 'image', image: 'cid:img', filename: 'test.png' },
    ]);
  });

  it('should not drop attachments on undefined content (regression test)', () => {
    const messages: any[] = [
      {
        role: 'user',
        content: undefined,
        attachments: [
          { content: [{ type: 'image', image: 'cid:img', filename: 'test.png' }] },
        ],
      },
    ];
    const result = toAgUiMessages(messages);
    expect(result[0].content).toEqual([
      { type: 'image', image: 'cid:img', filename: 'test.png' },
    ]);
  });

  it('should handle no attachments (string)', () => {
    const messages: any[] = [
      {
        role: 'user',
        content: 'hello',
      },
    ];
    expect(toAgUiMessages(messages)[0].content).toBe('hello');
  });

  it('should handle empty content', () => {
    const messages: any[] = [
      {
        role: 'user',
        content: '',
      },
    ];
    expect(toAgUiMessages(messages)[0].content).toBe('');
  });

  it('should handle multiple messages', () => {
    const messages: any[] = [
      { role: 'user', content: 'a' },
      { role: 'assistant', content: 'b' },
      { role: 'user', content: 'c', attachments: [{ content: [{ type: 'image', image: 'cid:img' }] }] },
    ];
    const result = toAgUiMessages(messages);
    expect(result).toHaveLength(3);
    expect(result[0].content).toBe('a');
    expect(result[1].content).toBe('b');
    expect(result[2].content).toEqual([
      { type: 'text', text: 'c' },
      { type: 'image', image: 'cid:img', filename: undefined },
    ]);
  });
});

  describe('HarnxHttpAgent', () => {
    it('handles custom events correctly', async () => {
      const onStatus = vi.fn();
      const onUsage = vi.fn();
      const onToolSummary = vi.fn();
      const onRunFailed = vi.fn();

      const { HarnxHttpAgent } = await import('../ChatProvider');
      const agent = new HarnxHttpAgent({
        url: '/url',
        onStatus,
        onRunFailed,
        onUsage,
        onToolSummary,
        onSubAgentEvent: vi.fn(),
      });

      const subscriber: any = {};
      vi.spyOn(Object.getPrototypeOf(Object.getPrototypeOf(agent)), 'runAgent').mockImplementation((_params: any, sub: any) => {
        Object.assign(subscriber, sub);
        return Promise.resolve();
      });

      await agent.runAgent({});

      // Simulate onEvent CUSTOM usage
      await subscriber.onEvent({
        event: {
          type: 'CUSTOM',
          name: 'usage',
          value: { input: 1, output: 2, context_tokens: 10 }
        }
      });
      expect(onUsage).toHaveBeenCalledWith({ input: 1, output: 2, context_tokens: 10 });

      // Simulate onEvent CUSTOM tool_summary
      await subscriber.onEvent({
        event: {
          type: 'CUSTOM',
          name: 'tool_summary',
          value: { tool_call_id: 'call_1', markdown: 'md' }
        }
      });
      expect(onToolSummary).toHaveBeenCalledWith('call_1', 'md');

      // Simulate onEvent CUSTOM status
      await subscriber.onEvent({
        event: {
          type: 'CUSTOM',
          name: 'status',
          value: { text: 'Running' }
        }
      });
      expect(onStatus).toHaveBeenCalledWith('Running');

      // Verify no double-dispatch if onCustomEvent is also called
      onStatus.mockClear();
      await subscriber.onEvent({
        event: {
          type: 'CUSTOM',
          name: 'status',
          value: { text: 'Running Again' }
        }
      });
      await subscriber.onCustomEvent?.({
        event: {
          type: 'CUSTOM',
          name: 'status',
          value: { text: 'Running Again' }
        }
      });
      expect(onStatus).toHaveBeenCalledTimes(1);
      expect(onStatus).toHaveBeenCalledWith('Running Again');

      // Simulate onEvent CUSTOM session_title_updated
      await subscriber.onEvent({
        event: {
          type: 'CUSTOM',
          name: 'session_title_updated',
          value: { title: 'New Test Title' }
        }
      });
      expect(document.title).toBe('harnx — New Test Title');

      await subscriber.onEvent({
        event: {
          type: 'CUSTOM',
          name: 'session_title_updated',
          value: { title: '' }
        }
      });
      expect(document.title).toBe('harnx');

      // Test missing fields tolerance (e.g. older server without context_tokens)
      await subscriber.onEvent({
        event: {
          type: 'CUSTOM',
          name: 'usage',
          value: { input: 1, output: 2 }
        }
      });
      expect(onUsage).toHaveBeenCalledWith({ input: 1, output: 2 });
    });

    it('should ignore malformed custom event payloads', async () => {
      const onUsage = vi.fn();
      const onToolSummary = vi.fn();
      const { handleHarnxCustomEvent } = await import('../harnxCustomEvents');
      const callbacks = {
        onStatus: vi.fn(),
        onRunFailed: vi.fn(),
        onUsage,
        onToolSummary,
      };

      handleHarnxCustomEvent('usage', { input: 'not-a-number', output: 2 }, callbacks);
      handleHarnxCustomEvent(
        'tool_summary',
        { tool_call_id: 'call_2', markdown: 42 },
        callbacks
      );

      expect(onUsage).not.toHaveBeenCalled();
      expect(onToolSummary).not.toHaveBeenCalled();
    });
    
    it('should gate session_handoff navigation on attach sequence boundary', async () => {
      const onStatus = vi.fn();
      const onUsage = vi.fn();
      const onToolSummary = vi.fn();
      const onRunFailed = vi.fn();
      const onHandoff = vi.fn();

      const { HarnxHttpAgent } = await import('../ChatProvider');
      const agent = new HarnxHttpAgent({
        url: '/url',
        onStatus,
        onRunFailed,
        onUsage,
        onToolSummary,
        onHandoff,
        onSubAgentEvent: vi.fn(),
      });

      const subscriber: any = {};
      vi.spyOn(Object.getPrototypeOf(Object.getPrototypeOf(agent)), 'runAgent').mockImplementation((_params: any, sub: any) => {
        Object.assign(subscriber, sub);
        return Promise.resolve();
      });

      await agent.runAgent({});

      // Wire order: RUN_STARTED, then session_attach_boundary
      await dispatchAgentEvent(subscriber, { type: 'RUN_STARTED' });
      await dispatchAgentEvent(subscriber, {
        type: 'CUSTOM',
        name: 'session_attach_boundary',
        value: { attached_seq: 10 }
      });

      // Replayed session_handoff (seq <= boundary) is skipped
      await dispatchAgentEvent(subscriber, {
        type: 'CUSTOM',
        name: 'session_handoff',
        value: { agent: 'targetAgent', session_id: '1234', after_seq: 10 }
      });
      expect(onHandoff).not.toHaveBeenCalled();

      // Strictly-newer live session_handoff (seq > boundary) navigates
      await dispatchAgentEvent(subscriber, {
        type: 'CUSTOM',
        name: 'session_handoff',
        value: { agent: 'targetAgent', session_id: '1234', after_seq: 11 }
      });
      expect(onHandoff).toHaveBeenCalledWith('targetAgent', '1234');

      // Duplicated session_handoff frame navigates at most once
      await dispatchAgentEvent(subscriber, {
        type: 'CUSTOM',
        name: 'session_handoff',
        value: { agent: 'targetAgent', session_id: '1234', after_seq: 11 }
      });
      expect(onHandoff).toHaveBeenCalledTimes(1);

      onHandoff.mockClear();

      // Uncommitted/invalid payloads must never navigate.
      await dispatchAgentEvent(subscriber, {
        type: 'CUSTOM',
        name: 'session_handoff',
        value: { agent: 'targetAgent', session_id: null, after_seq: 12 }
      });
      expect(onHandoff).not.toHaveBeenCalled();

      await dispatchAgentEvent(subscriber, {
        type: 'CUSTOM',
        name: 'session_handoff',
        value: { agent: 'targetAgent', session_id: '   ', after_seq: 12 }
      });
      expect(onHandoff).not.toHaveBeenCalled();

      await dispatchAgentEvent(subscriber, {
        type: 'CUSTOM',
        name: 'turn_handoff_requested',
        value: { agent: 'targetAgent', session_id: 'request-only', after_seq: 12 }
      });
      expect(onHandoff).not.toHaveBeenCalled();

      // Missing afterSeq or non-integer must never navigate
      await dispatchAgentEvent(subscriber, {
        type: 'CUSTOM',
        name: 'session_handoff',
        value: { agent: 'targetAgent', session_id: '9999' }
      });
      expect(onHandoff).not.toHaveBeenCalled();

      await dispatchAgentEvent(subscriber, {
        type: 'CUSTOM',
        name: 'session_handoff',
        value: { agent: 'targetAgent', session_id: '9999', after_seq: 'invalid' }
      });
      expect(onHandoff).not.toHaveBeenCalled();

      // Strictly newer live handoff afterSeq 12 navigates
      await dispatchAgentEvent(subscriber, {
        type: 'CUSTOM',
        name: 'session_handoff',
        value: { agent: 'targetAgent3', session_id: '9999', after_seq: 12 }
      });
      expect(onHandoff).toHaveBeenCalledWith('targetAgent3', '9999');
    });

    it('reports transport failures to connection.noteTransientTrouble without replaying', async () => {
      const { connection } = await import('../connection');
      const noteSpy = vi.spyOn(connection, 'noteTransientTrouble');

      const onRunFailed = vi.fn();
      const { HarnxHttpAgent } = await import('../ChatProvider');
      const agent = new HarnxHttpAgent({
        url: '/url',
        onStatus: vi.fn(),
        onRunFailed,
        onUsage: vi.fn(),
        onToolSummary: vi.fn(),
        onSubAgentEvent: vi.fn(),
      });

      // 1. Transport error in runAgent catch block
      vi.spyOn(Object.getPrototypeOf(Object.getPrototypeOf(agent)), 'runAgent').mockRejectedValueOnce(
        new TypeError('Failed to fetch')
      );

      await expect(agent.runAgent({})).rejects.toThrow('Failed to fetch');
      expect(noteSpy).toHaveBeenCalledTimes(1);
      expect(onRunFailed).toHaveBeenCalledWith('Failed to fetch');

      // 2. Non-transport / model / application error should NOT note trouble
      noteSpy.mockClear();
      onRunFailed.mockClear();

      vi.spyOn(Object.getPrototypeOf(Object.getPrototypeOf(agent)), 'runAgent').mockRejectedValueOnce(
        new Error('Context length exceeded: prompt too long')
      );

      await expect(agent.runAgent({})).rejects.toThrow('Context length exceeded');
      expect(noteSpy).not.toHaveBeenCalled();
      expect(onRunFailed).toHaveBeenCalledWith('Context length exceeded: prompt too long');

      // 3. onRunFailed subscriber callback with transport error
      noteSpy.mockClear();
      onRunFailed.mockClear();

      const subscriber: any = {};
      vi.spyOn(Object.getPrototypeOf(Object.getPrototypeOf(agent)), 'runAgent').mockImplementation((_params: any, sub: any) => {
        Object.assign(subscriber, sub);
        return Promise.resolve();
      });

      await agent.runAgent({});
      await subscriber.onRunFailed({ error: new Error('net::ERR_CONNECTION_REFUSED') });
      expect(noteSpy).toHaveBeenCalledTimes(1);
      expect(onRunFailed).toHaveBeenCalledWith('net::ERR_CONNECTION_REFUSED');

      // 4. onRunFailed subscriber callback with application error
      noteSpy.mockClear();
      onRunFailed.mockClear();
      await subscriber.onRunFailed({ error: new Error('Rate limit exceeded: 429') });
      expect(noteSpy).not.toHaveBeenCalled();
      expect(onRunFailed).toHaveBeenCalledWith('Rate limit exceeded: 429');
    });

    it('suppresses onRunFailed and RUN_ERROR for AbortError, TimeoutError, and "signal is aborted without reason" (#1838, #1861)', async () => {
      const onRunFailed = vi.fn();
      const onSubAgentEvent = vi.fn();
      const { HarnxHttpAgent } = await import('../ChatProvider');
      const agent = new HarnxHttpAgent({
        url: '/url',
        onStatus: vi.fn(),
        onRunFailed,
        onUsage: vi.fn(),
        onToolSummary: vi.fn(),
        onSubAgentEvent,
      });

      // 1. AbortError DOMException
      (agent as any).handleRunFailure('Aborted', new DOMException('The operation was aborted', 'AbortError'));
      expect(onSubAgentEvent).not.toHaveBeenCalled();
      expect(onRunFailed).not.toHaveBeenCalled();

      // 2. TimeoutError DOMException
      (agent as any).handleRunFailure('Timeout', new DOMException('The operation was aborted due to timeout', 'TimeoutError'));
      expect(onSubAgentEvent).not.toHaveBeenCalled();
      expect(onRunFailed).not.toHaveBeenCalled();

      // 3. "signal is aborted without reason" message
      (agent as any).handleRunFailure('signal is aborted without reason');
      expect(onSubAgentEvent).not.toHaveBeenCalled();
      expect(onRunFailed).not.toHaveBeenCalled();

      // 4. "signal is aborted without reason" Error instance
      (agent as any).handleRunFailure('Failed', new Error('signal is aborted without reason'));
      expect(onSubAgentEvent).not.toHaveBeenCalled();
      expect(onRunFailed).not.toHaveBeenCalled();

      // 5. Genuine transport error still emits RUN_ERROR and calls onRunFailed
      (agent as any).handleRunFailure('Failed to fetch', new TypeError('Failed to fetch'));
      expect(onSubAgentEvent).toHaveBeenCalledWith({ type: 'RUN_ERROR' });
      expect(onRunFailed).toHaveBeenCalledWith('Failed to fetch');

      // 6. Genuine application error still emits RUN_ERROR and calls onRunFailed
      onSubAgentEvent.mockClear();
      onRunFailed.mockClear();
      (agent as any).handleRunFailure('Model error: context length exceeded', new Error('Model error'));
      expect(onSubAgentEvent).toHaveBeenCalledWith({ type: 'RUN_ERROR' });
      expect(onRunFailed).toHaveBeenCalledWith('Model error: context length exceeded');
    });

    it('does not forward message_attachments custom events to the assistant-ui runtime', async () => {
      const { HarnxHttpAgent } = await import('../ChatProvider');
      const agent = new HarnxHttpAgent({
        url: '/url',
        onStatus: vi.fn(),
        onRunFailed: vi.fn(),
        onUsage: vi.fn(),
        onToolSummary: vi.fn(),
        onSubAgentEvent: vi.fn(),
      });

      expect((agent as any).forwardsCustomEvent('message_attachments')).toBe(false);
      expect((agent as any).forwardsCustomEvent('session_attach_boundary')).toBe(false);
      expect((agent as any).forwardsCustomEvent('session_handoff')).toBe(false);
      expect((agent as any).forwardsCustomEvent('pending_interaction')).toBe(false);
      expect((agent as any).forwardsCustomEvent('other_event')).toBe(true);
    });

    it('passes pending_interaction to onPendingInteraction via handleAgentEvent', async () => {
      const onPendingInteraction = vi.fn();
      const { HarnxHttpAgent } = await import('../ChatProvider');
      const agent = new HarnxHttpAgent({
        url: '/url',
        onStatus: vi.fn(),
        onRunFailed: vi.fn(),
        onUsage: vi.fn(),
        onToolSummary: vi.fn(),
        onSubAgentEvent: vi.fn(),
        onPendingInteraction,
      });

      (agent as any).handleAgentEvent({
        type: 'CUSTOM',
        name: 'pending_interaction',
        value: {
          interaction_id: 'int-789',
          request_seq: 1,
          tool_call_id: 'tc-1',
          kind: 'confirm',
          message: 'Confirm execution',
          pending_count: 0,
        },
      });

      expect(onPendingInteraction).toHaveBeenCalledWith({
        interactionId: 'int-789',
        requestSeq: 1,
        toolCallId: 'tc-1',
        kind: 'confirm',
        message: 'Confirm execution',
        pendingCount: 0,
      });

      // Test clearing via null
      (agent as any).handleAgentEvent({
        type: 'CUSTOM',
        name: 'pending_interaction',
        value: null,
      });

      expect(onPendingInteraction).toHaveBeenCalledWith(null);
    });

    it('passes message_attachments to onMessageAttachments via handleAgentEvent', async () => {
      const onMessageAttachments = vi.fn();
      const { HarnxHttpAgent } = await import('../ChatProvider');
      const agent = new HarnxHttpAgent({
        url: '/url',
        onStatus: vi.fn(),
        onRunFailed: vi.fn(),
        onUsage: vi.fn(),
        onToolSummary: vi.fn(),
        onSubAgentEvent: vi.fn(),
        onMessageAttachments,
      });

      (agent as any).handleAgentEvent({
        type: 'CUSTOM',
        name: 'message_attachments',
        value: {
          messageId: 'msg-1',
          attachments: [{ partIndex: 0, cid: 'cid:sha123', kind: 'image' }],
        },
      });

      expect(onMessageAttachments).toHaveBeenCalledWith('msg-1', [
        { partIndex: 0, cid: 'cid:sha123', kind: 'image' },
      ]);
    });

    it('invokes onRunTerminal on RUN_FINISHED, RUN_ERROR, turn_interrupted, and handleRunFailure', async () => {
      const onRunTerminal = vi.fn();
      const onTurnInterrupted = vi.fn();
      const { HarnxHttpAgent } = await import('../ChatProvider');
      const agent = new HarnxHttpAgent({
        url: '/url',
        onStatus: vi.fn(),
        onRunFailed: vi.fn(),
        onUsage: vi.fn(),
        onToolSummary: vi.fn(),
        onSubAgentEvent: vi.fn(),
        onRunTerminal,
        onTurnInterrupted,
      });

      // RUN_FINISHED
      (agent as any).handleAgentEvent({ type: 'RUN_FINISHED' });
      expect(onRunTerminal).toHaveBeenCalledTimes(1);

      // RUN_ERROR
      (agent as any).handleAgentEvent({ type: 'RUN_ERROR', message: 'Failed' });
      expect(onRunTerminal).toHaveBeenCalledTimes(2);

      // turn_interrupted
      (agent as any).handleAgentEvent({ type: 'CUSTOM', name: 'turn_interrupted' });
      expect(onRunTerminal).toHaveBeenCalledTimes(3);
      expect(onTurnInterrupted).toHaveBeenCalledTimes(1);

      // handleRunFailure
      (agent as any).handleRunFailure('fatal error', new Error('fatal'));
      expect(onRunTerminal).toHaveBeenCalledTimes(4);
    });

    it('sends the x-harnx-snapshot-features: history-reasoning opt-in header only for same-origin URLs and suppresses forwarding history_reasoning', async () => {
      const { HarnxHttpAgent } = await import('../ChatProvider');
      const sameOriginAgent = new HarnxHttpAgent({
        url: '/url',
        onStatus: vi.fn(),
        onRunFailed: vi.fn(),
        onUsage: vi.fn(),
        onToolSummary: vi.fn(),
        onSubAgentEvent: vi.fn(),
      });

      expect(sameOriginAgent.headers['x-harnx-snapshot-features']).toBe('history-reasoning');
      expect((sameOriginAgent as any).forwardsCustomEvent('history_reasoning')).toBe(false);

      const crossOriginAgent = new HarnxHttpAgent({
        url: 'https://cross-origin.example/v1/agents/a/sessions/s',
        onStatus: vi.fn(),
        onRunFailed: vi.fn(),
        onUsage: vi.fn(),
        onToolSummary: vi.fn(),
        onSubAgentEvent: vi.fn(),
      });

      expect(crossOriginAgent.headers['x-harnx-snapshot-features']).toBeUndefined();
    });

    it('inserts reasoning messages before owning assistant rows when history_reasoning follows MESSAGES_SNAPSHOT', async () => {
      const { HarnxHttpAgent } = await import('../ChatProvider');
      const agent = new HarnxHttpAgent({
        url: '/url',
        onStatus: vi.fn(),
        onRunFailed: vi.fn(),
        onUsage: vi.fn(),
        onToolSummary: vi.fn(),
        onSubAgentEvent: vi.fn(),
      });

      const wrappedRef: any = {};
      let resolveRun!: () => void;
      const runAgentSpy = vi.spyOn(Object.getPrototypeOf(Object.getPrototypeOf(agent)), 'runAgent').mockImplementation((_params: any, sub: any) => {
        Object.assign(wrappedRef, sub);
        return new Promise<void>((resolve) => {
          resolveRun = resolve;
        });
      });

      try {
        const onMessagesSnapshotEvent = vi.fn();
        const onCustomEvent = vi.fn();
        const onRunFinishedEvent = vi.fn();
        const runPromise = agent.runAgent({}, {
          onMessagesSnapshotEvent,
          onCustomEvent,
          onRunFinishedEvent,
        });

        const snapshotEvent = {
          type: 'MESSAGES_SNAPSHOT',
          messages: [
            { id: 'u1', role: 'user', content: 'Question' },
            { id: 'a1', role: 'assistant', content: 'Answer' },
            { id: 'a2', role: 'assistant', content: '' },
          ],
        };

        await wrappedRef.onEvent({ event: snapshotEvent });
        await wrappedRef.onMessagesSnapshotEvent({ event: snapshotEvent });
        expect(onMessagesSnapshotEvent).not.toHaveBeenCalled();

        const reasoningEvent = {
          type: 'CUSTOM',
          name: 'history_reasoning',
          value: {
            a1: ['First thought', 'Second thought'],
            a2: ['Thought-only row'],
          },
        };
        await wrappedRef.onEvent({ event: reasoningEvent });
        const customMutation = await wrappedRef.onCustomEvent({ event: reasoningEvent });

        expect(onCustomEvent).not.toHaveBeenCalled();
        expect(onMessagesSnapshotEvent).toHaveBeenCalledTimes(1);
        const augmentedMessages = [
          { id: 'u1', role: 'user', content: 'Question' },
          { id: 'a1:reasoning-0', role: 'reasoning', content: 'First thought' },
          { id: 'a1:reasoning-1', role: 'reasoning', content: 'Second thought' },
          { id: 'a1', role: 'assistant', content: 'Answer' },
          { id: 'a2:reasoning-0', role: 'reasoning', content: 'Thought-only row' },
          { id: 'a2', role: 'assistant', content: '' },
        ];
        expect(onMessagesSnapshotEvent.mock.calls[0][0].event.messages).toEqual(augmentedMessages);
        expect(customMutation).toEqual({ messages: augmentedMessages });

        // Empty map on a subsequent snapshot replaces prior reasoning cleanly and removes stale reasoning from agent state.
        onMessagesSnapshotEvent.mockClear();
        const snapshot2 = {
          type: 'MESSAGES_SNAPSHOT',
          messages: [{ id: 'a1', role: 'assistant', content: 'Answer' }],
        };
        await wrappedRef.onEvent({ event: snapshot2 });
        await wrappedRef.onMessagesSnapshotEvent({ event: snapshot2 });
        const emptyReasoning = { type: 'CUSTOM', name: 'history_reasoning', value: {} };
        await wrappedRef.onEvent({ event: emptyReasoning });
        const emptyMutation = await wrappedRef.onCustomEvent({ event: emptyReasoning });
        expect(onMessagesSnapshotEvent).toHaveBeenCalledTimes(1);
        expect(onMessagesSnapshotEvent.mock.calls[0][0].event.messages).toEqual([
          { id: 'a1', role: 'assistant', content: 'Answer' },
        ]);
        expect(emptyMutation).toEqual({
          messages: [{ id: 'a1', role: 'assistant', content: 'Answer' }],
        });

        // Legacy server without history_reasoning flushes snapshot before RUN_FINISHED.
        onMessagesSnapshotEvent.mockClear();
        await wrappedRef.onEvent({ event: snapshot2 });
        await wrappedRef.onMessagesSnapshotEvent({ event: snapshot2 });
        const finished = { type: 'RUN_FINISHED' };
        await wrappedRef.onEvent({ event: finished });
        await wrappedRef.onRunFinishedEvent({ event: finished });
        expect(onMessagesSnapshotEvent).toHaveBeenCalledTimes(1);
        expect(onRunFinishedEvent).toHaveBeenCalledTimes(1);

        resolveRun();
        await runPromise;
      } finally {
        runAgentSpy.mockRestore();
      }
    });

    it('flushes buffered MESSAGES_SNAPSHOT after short timeout on an idle legacy open stream and still hydrates late history_reasoning', async () => {
      vi.useFakeTimers();
      let runAgentSpy: { mockRestore: () => void } | undefined;
      try {
        const { HarnxHttpAgent } = await import('../ChatProvider');
        const agent = new HarnxHttpAgent({
          url: '/url',
          onStatus: vi.fn(),
          onRunFailed: vi.fn(),
          onUsage: vi.fn(),
          onToolSummary: vi.fn(),
          onSubAgentEvent: vi.fn(),
        });

        const wrappedRef: any = {};
        let resolveRun!: () => void;
        runAgentSpy = vi.spyOn(Object.getPrototypeOf(Object.getPrototypeOf(agent)), 'runAgent').mockImplementation((_params: any, sub: any) => {
          Object.assign(wrappedRef, sub);
          return new Promise<void>((resolve) => {
            resolveRun = resolve;
          });
        });

        const onMessagesSnapshotEvent = vi.fn();
        const runPromise = agent.runAgent({}, { onMessagesSnapshotEvent });

        const snapshotEvent = {
          type: 'MESSAGES_SNAPSHOT',
          messages: [
            { id: 'u1', role: 'user', content: 'Question' },
            { id: 'a1', role: 'assistant', content: 'Answer' },
          ],
        };

        await wrappedRef.onEvent({ event: snapshotEvent });
        await wrappedRef.onMessagesSnapshotEvent({ event: snapshotEvent });
        expect(onMessagesSnapshotEvent).not.toHaveBeenCalled();

        // Advance past the bounded fallback window without any following event.
        await vi.advanceTimersByTimeAsync(60);

        expect(onMessagesSnapshotEvent).toHaveBeenCalledTimes(1);
        expect(onMessagesSnapshotEvent.mock.calls[0][0].event.messages).toEqual([
          { id: 'u1', role: 'user', content: 'Question' },
          { id: 'a1', role: 'assistant', content: 'Answer' },
        ]);

        // If a delayed history_reasoning event arrives afterwards on an otherwise idle stream,
        // it re-dispatches the snapshot with the reasoning rows injected.
        const lateReasoning = {
          type: 'CUSTOM',
          name: 'history_reasoning',
          value: { a1: ['Late thought'] },
        };
        await wrappedRef.onEvent({ event: lateReasoning });
        await wrappedRef.onCustomEvent({ event: lateReasoning });

        expect(onMessagesSnapshotEvent).toHaveBeenCalledTimes(2);
        expect(onMessagesSnapshotEvent.mock.calls[1][0].event.messages).toEqual([
          { id: 'u1', role: 'user', content: 'Question' },
          { id: 'a1:reasoning-0', role: 'reasoning', content: 'Late thought' },
          { id: 'a1', role: 'assistant', content: 'Answer' },
        ]);

        resolveRun();
        await runPromise;
      } finally {
        runAgentSpy?.mockRestore();
        vi.useRealTimers();
      }
    });

    it('catches throwing snapshot callbacks so run-failure reporting and fallback timer do not drop errors or reject', async () => {
      vi.useFakeTimers();
      const consoleErrorSpy = vi.spyOn(console, 'error').mockImplementation(() => {});
      let runAgentSpy: { mockRestore: () => void } | undefined;
      try {
        const { HarnxHttpAgent } = await import('../ChatProvider');
        const onRunFailed = vi.fn();
        const onRunTerminal = vi.fn();
        const agent = new HarnxHttpAgent({
          url: '/url',
          onStatus: vi.fn(),
          onRunFailed,
          onUsage: vi.fn(),
          onToolSummary: vi.fn(),
          onSubAgentEvent: vi.fn(),
          onRunTerminal,
        });

        const wrappedRef: any = {};
        let resolveRun!: () => void;
        runAgentSpy = vi.spyOn(Object.getPrototypeOf(Object.getPrototypeOf(agent)), 'runAgent').mockImplementation((_params: any, sub: any) => {
          Object.assign(wrappedRef, sub);
          return new Promise<void>((resolve) => {
            resolveRun = resolve;
          });
        });

        const downstreamRunFailed = vi.fn();
        const onMessagesSnapshotEvent = vi.fn().mockRejectedValue(new Error('snapshot callback boom'));
        const runPromise = agent.runAgent({}, {
          onMessagesSnapshotEvent,
          onRunFailed: downstreamRunFailed,
        });

        const snapshotEvent = {
          type: 'MESSAGES_SNAPSHOT',
          messages: [{ id: 'a1', role: 'assistant', content: 'Answer' }],
        };

        // 1. Timer flush rejects inside callback -> caught, logged, no unhandled rejection.
        await wrappedRef.onEvent({ event: snapshotEvent });
        await wrappedRef.onMessagesSnapshotEvent({ event: snapshotEvent });
        await vi.advanceTimersByTimeAsync(60);
        expect(onMessagesSnapshotEvent).toHaveBeenCalledTimes(1);
        expect(consoleErrorSpy).toHaveBeenCalled();

        // 2. Pending snapshot flushed during onRunFailed throws -> run-failure reporting still runs.
        await wrappedRef.onEvent({ event: snapshotEvent });
        await wrappedRef.onMessagesSnapshotEvent({ event: snapshotEvent });
        await wrappedRef.onRunFailed({ error: new Error('stream dropped') });
        expect(onMessagesSnapshotEvent).toHaveBeenCalledTimes(2);
        expect(onRunTerminal).toHaveBeenCalledTimes(1);
        expect(onRunFailed).toHaveBeenCalledWith('stream dropped');
        expect(downstreamRunFailed).toHaveBeenCalledTimes(1);

        resolveRun();
        await runPromise;
      } finally {
        runAgentSpy?.mockRestore();
        consoleErrorSpy.mockRestore();
        vi.useRealTimers();
      }
    });

    it('drops stale synthetic role:reasoning rows from agent.messages when a snapshot is followed by history_reasoning: {}', async () => {
      const { HarnxHttpAgent } = await import('../ChatProvider');
      const agent = new HarnxHttpAgent({
        url: '/url',
        onStatus: vi.fn(),
        onRunFailed: vi.fn(),
        onUsage: vi.fn(),
        onToolSummary: vi.fn(),
        onSubAgentEvent: vi.fn(),
      });

      const sseFrame = (event: unknown) =>
        `event: message\ndata: ${JSON.stringify(event)}\n\n`;
      const body = [
        sseFrame({ type: 'RUN_STARTED', threadId: 't1', runId: 'r1' }),
        sseFrame({
          type: 'MESSAGES_SNAPSHOT',
          messages: [
            { id: 'u1', role: 'user', content: 'Solve it' },
            { id: 'a1', role: 'assistant', content: 'Answer' },
          ],
        }),
        sseFrame({
          type: 'CUSTOM',
          name: 'history_reasoning',
          value: { a1: ['First thought'] },
        }),
        sseFrame({
          type: 'MESSAGES_SNAPSHOT',
          messages: [
            { id: 'summary', role: 'system', content: 'Compacted summary' },
          ],
        }),
        sseFrame({
          type: 'CUSTOM',
          name: 'history_reasoning',
          value: {},
        }),
        sseFrame({ type: 'RUN_FINISHED', threadId: 't1', runId: 'r1' }),
      ].join('');

      const fetchSpy = vi.spyOn(globalThis, 'fetch').mockResolvedValueOnce(
        new Response(body, {
          status: 200,
          headers: { 'Content-Type': 'text/event-stream' },
        }),
      );

      try {
        await agent.runAgent({ threadId: 't1', runId: 'r1', messages: [] }, {});
        expect(agent.messages.map((m) => ({ id: m.id, role: m.role }))).toEqual([
          { id: 'summary', role: 'system' },
        ]);
        expect(agent.messages.some((m) => (m.role as string) === 'reasoning')).toBe(false);
      } finally {
        fetchSpy.mockRestore();
      }
    });

    it('preserves pendingIdleMessagesSync when late history_reasoning: {} arrives after the 50ms fallback flush', async () => {
      vi.useFakeTimers();
      const encoder = new TextEncoder();
      const sseFrame = (event: unknown) =>
        encoder.encode(`event: message\ndata: ${JSON.stringify(event)}\n\n`);

      let controller!: ReadableStreamDefaultController<Uint8Array>;
      const stream = new ReadableStream<Uint8Array>({
        start(c) {
          controller = c;
        },
      });

      const fetchSpy = vi.spyOn(globalThis, 'fetch').mockResolvedValueOnce(
        new Response(stream, {
          status: 200,
          headers: { 'Content-Type': 'text/event-stream' },
        }),
      );

      try {
        const { HarnxHttpAgent } = await import('../ChatProvider');
        const agent = new HarnxHttpAgent({
          url: '/url',
          onStatus: vi.fn(),
          onRunFailed: vi.fn(),
          onUsage: vi.fn(),
          onToolSummary: vi.fn(),
          onSubAgentEvent: vi.fn(),
        });

        const runPromise = agent.runAgent({ threadId: 't1', runId: 'r1', messages: [] }, {});

        // Initial snapshot + reasoning puts synthetic role:"reasoning" into @ag-ui/client's local messages.
        controller.enqueue(sseFrame({ type: 'RUN_STARTED', threadId: 't1', runId: 'r1' }));
        controller.enqueue(sseFrame({
          type: 'MESSAGES_SNAPSHOT',
          messages: [
            { id: 'u1', role: 'user', content: 'Solve it' },
            { id: 'a1', role: 'assistant', content: 'Answer' },
          ],
        }));
        controller.enqueue(sseFrame({
          type: 'CUSTOM',
          name: 'history_reasoning',
          value: { a1: ['Old thought'] },
        }));
        await vi.advanceTimersByTimeAsync(10);

        // Compaction snapshot arrives, then stalls past the 50 ms fallback timer.
        controller.enqueue(sseFrame({
          type: 'MESSAGES_SNAPSHOT',
          messages: [{ id: 'summary', role: 'system', content: 'Compacted summary' }],
        }));
        await vi.advanceTimersByTimeAsync(60);

        // Late empty history_reasoning arrives after the fallback flush, followed by another mutating event.
        controller.enqueue(sseFrame({
          type: 'CUSTOM',
          name: 'history_reasoning',
          value: {},
        }));
        controller.enqueue(sseFrame({
          type: 'TEXT_MESSAGE_START',
          messageId: 'm2',
          role: 'assistant',
        }));
        controller.enqueue(sseFrame({
          type: 'TEXT_MESSAGE_CONTENT',
          messageId: 'm2',
          delta: 'After compaction',
        }));
        controller.enqueue(sseFrame({
          type: 'TEXT_MESSAGE_END',
          messageId: 'm2',
        }));
        controller.enqueue(sseFrame({ type: 'RUN_FINISHED', threadId: 't1', runId: 'r1' }));
        controller.close();

        await vi.runAllTimersAsync();
        await runPromise;

        expect(agent.messages.map((m) => ({ id: m.id, role: m.role }))).toEqual([
          { id: 'summary', role: 'system' },
          { id: 'm2', role: 'assistant' },
        ]);
        expect(agent.messages.some((m) => (m.role as string) === 'reasoning')).toBe(false);
      } finally {
        fetchSpy.mockRestore();
        vi.useRealTimers();
      }
    });
  });
