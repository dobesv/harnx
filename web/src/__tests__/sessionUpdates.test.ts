import { describe, expect, it } from 'vitest';
import { isPromptlessRun } from '../mocks/sessionUpdates';

describe('isPromptlessRun', () => {
  it('hydrates when the trailing user message is already persisted', () => {
    const messages = [{ id: 'persisted-user', role: 'user' }];

    expect(isPromptlessRun(messages, messages)).toBe(true);
  });

  it('prompts when the trailing user message is not persisted', () => {
    expect(isPromptlessRun(
      [{ id: 'new-user', role: 'user' }],
      [{ id: 'prior-user' }],
    )).toBe(false);
  });

  it('hydrates when the transcript does not end in a user message', () => {
    expect(isPromptlessRun([{ id: 'reply', role: 'assistant' }], [])).toBe(true);
  });
});
