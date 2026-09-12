import { describe, it, expect } from 'vitest';
import { getToolCallPresentation } from './toolCallPresentation';

describe('getToolCallPresentation', () => {
  const cases = [
    {
      scenario: 'running',
      status: { type: 'running' },
      isError: undefined,
      expected: { icon: '⏳', borderColor: 'var(--status-running)', defaultExpanded: false },
    },
    {
      scenario: 'requires-action + reason tool-calls (pending in a reconstructed tab)',
      status: { type: 'requires-action', reason: 'tool-calls' },
      isError: undefined,
      expected: { icon: '⏳', borderColor: 'var(--status-running)', defaultExpanded: false },
    },
    {
      scenario: 'requires-action + reason omitted (defensively treated as pending)',
      status: { type: 'requires-action' },
      isError: undefined,
      expected: { icon: '⏳', borderColor: 'var(--status-running)', defaultExpanded: false },
    },
    {
      scenario: 'requires-action + reason interrupt (genuine approval alert)',
      status: { type: 'requires-action', reason: 'interrupt' },
      isError: undefined,
      expected: { icon: '⚠️', borderColor: 'var(--status-action)', defaultExpanded: true },
    },
    {
      scenario: 'complete without error',
      status: { type: 'complete' },
      isError: undefined,
      expected: { icon: '✅', borderColor: 'var(--status-complete)', defaultExpanded: false },
    },
    {
      scenario: 'complete with isError (icon reflects error, border stays complete)',
      status: { type: 'complete' },
      isError: true,
      expected: { icon: '❌', borderColor: 'var(--status-complete)', defaultExpanded: false },
    },
    {
      scenario: 'incomplete with error',
      status: { type: 'incomplete' },
      isError: true,
      expected: { icon: '❌', borderColor: 'var(--status-error)', defaultExpanded: false },
    },
    {
      scenario: 'undefined status',
      status: undefined,
      isError: undefined,
      expected: { icon: '✅', borderColor: 'var(--border)', defaultExpanded: false },
    },
  ] as const;

  it.each(cases)('maps $scenario', ({ status, isError, expected }) => {
    expect(getToolCallPresentation(status, isError)).toEqual(expected);
  });
});
