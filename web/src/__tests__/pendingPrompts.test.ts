import { describe, expect, it, vi } from 'vitest';
import { createPendingPrompt, pendingUserMessageIds, reconcilePendingPrompts, type PendingPromptsState } from '../pendingPrompts';

describe('queued prompt reconciliation', () => {
  it('does not scan the transcript when no prompts are pending', () => {
    const messages = new Proxy([], {
      get() { throw new Error('transcript accessed'); },
    });
    const state = { prompts: [] };
    expect(pendingUserMessageIds(state, messages)).toBeNull();
    expect(reconcilePendingPrompts(state, null)).toBe(state);
  });

  it('creates unique local rows without secure-context crypto APIs', () => {
    const randomUUID = vi.spyOn(crypto, 'randomUUID').mockImplementation(() => {
      throw new Error('randomUUID unavailable');
    });
    try {
      const first = createPendingPrompt('hello', [], null);
      const second = createPendingPrompt('hello', [], null);
      expect(first.id).not.toBe(second.id);
    } finally {
      randomUUID.mockRestore();
    }
  });

  it('keeps a prompt visible until its exact server row appears', () => {
    const state: PendingPromptsState = {
      prompts: [{ id: 'pending', messageId: 'ours', text: 'hello', attachmentNames: [] }],
    };
    expect(reconcilePendingPrompts(state, ['old'])).toBe(state);
    // Another tab's identical text must not retire our queued row.
    expect(reconcilePendingPrompts(state, ['other-client-message'])).toBe(state);
    expect(reconcilePendingPrompts(state, ['ours']).prompts).toEqual([]);
  });

  it('consumes only one server row for identical queued prompts', () => {
    const prompt = { text: 'repeat', attachmentNames: [] };
    const state: PendingPromptsState = {
      prompts: [
        { ...prompt, id: 'first', messageId: 'server-1' },
        { ...prompt, id: 'second', messageId: 'server-2' },
      ],
    };
    const afterFirst = reconcilePendingPrompts(state, ['server-1']);
    expect(afterFirst.prompts.map((p) => p.id)).toEqual(['second']);
    const afterSecond = reconcilePendingPrompts(afterFirst, ['server-1', 'server-2']);
    expect(afterSecond.prompts).toEqual([]);
  });

  it('keeps an accepted legacy prompt visible without a message ID', () => {
    const state: PendingPromptsState = {
      prompts: [{ id: 'legacy', messageId: null, text: 'hello', attachmentNames: [] }],
    };
    expect(reconcilePendingPrompts(state, ['unrelated-server-row'])).toBe(state);
  });
});
