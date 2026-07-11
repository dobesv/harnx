import { describe, it, expect, vi } from 'vitest';
import { attachmentToMessageParts, toAgUiMessages } from '../ChatProvider';
import type { Message } from '@ag-ui/client';

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
        onToolSummary
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

      // Simulate onCustomEvent status
      await subscriber.onCustomEvent({
        event: {
          name: 'status',
          value: { text: 'Running' }
        }
      });
      expect(onStatus).toHaveBeenCalledWith('Running');

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
  });