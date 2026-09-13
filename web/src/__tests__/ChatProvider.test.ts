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
  });
