import { describe, it, expect } from 'vitest';
import { extractResultContent, getToolCallPresentation, isSubAgentTool } from './toolCallPresentation';

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

  describe('sub-agent default expansion and status presentation', () => {
    const subAgentCases = [
      {
        scenario: 'sub-agent running starts expanded with spinner',
        status: { type: 'running' },
        isError: undefined,
        options: { isSubAgent: true },
        expected: { icon: '⏳', borderColor: 'var(--status-running)', defaultExpanded: true },
      },
      {
        scenario: 'sub-agent complete without error starts expanded with checkmark',
        status: { type: 'complete' },
        isError: undefined,
        options: { isSubAgent: true },
        expected: { icon: '✅', borderColor: 'var(--status-complete)', defaultExpanded: true },
      },
      {
        scenario: 'sub-agent complete with error starts expanded with cross',
        status: { type: 'complete' },
        isError: true,
        options: { isSubAgent: true },
        expected: { icon: '❌', borderColor: 'var(--status-complete)', defaultExpanded: true },
      },
      {
        scenario: 'sub-agent incomplete with error starts expanded with error border',
        status: { type: 'incomplete' },
        isError: true,
        options: { isSubAgent: true },
        expected: { icon: '❌', borderColor: 'var(--status-error)', defaultExpanded: true },
      },
      {
        scenario: 'sub-agent identified via toolName option',
        status: { type: 'complete' },
        isError: undefined,
        options: { toolName: 'researcher_session_prompt' },
        expected: { icon: '✅', borderColor: 'var(--status-complete)', defaultExpanded: true },
      },
      {
        scenario: 'regular tool identified via toolName option stays collapsed by default',
        status: { type: 'complete' },
        isError: undefined,
        options: { toolName: 'bash_exec' },
        expected: { icon: '✅', borderColor: 'var(--status-complete)', defaultExpanded: false },
      },
    ] as const;

    it.each(subAgentCases)('maps $scenario', ({ status, isError, options, expected }) => {
      expect(getToolCallPresentation(status, isError, options)).toEqual(expected);
    });
  });

  describe('isSubAgentTool', () => {
    it('identifies sub-agent tools correctly', () => {
      expect(isSubAgentTool('session_prompt')).toBe(true);
      expect(isSubAgentTool('researcher_session_prompt')).toBe(true);
      expect(isSubAgentTool('pantheon__plato_session_prompt')).toBe(true);
      expect(isSubAgentTool('session_new')).toBe(true);
      expect(isSubAgentTool('agent_session_new')).toBe(true);

      expect(isSubAgentTool('bash_exec')).toBe(false);
      expect(isSubAgentTool('fs_read')).toBe(false);
      expect(isSubAgentTool('')).toBe(false);
      expect(isSubAgentTool(undefined)).toBe(false);
    });
  });

  describe('extractResultContent', () => {
    it('returns null text for undefined or null result without throwing (B2)', () => {
      expect(extractResultContent(undefined)).toEqual({
        text: null,
        isStructuredObject: false,
        structuredData: null,
      });
      expect(extractResultContent(null)).toEqual({
        text: null,
        isStructuredObject: false,
        structuredData: null,
      });
    });

    it('extracts plain text and markdown string results', () => {
      expect(extractResultContent('plain output')).toEqual({
        text: 'plain output',
        isStructuredObject: false,
        structuredData: { result: 'plain output' },
      });
      expect(extractResultContent('# Title\n- item')).toEqual({
        text: '# Title\n- item',
        isStructuredObject: false,
        structuredData: { result: '# Title\n- item' },
      });
    });

    const singleFieldCases = [
      { field: 'response', raw: { response: 'child done' }, json: '{"response":"child done"}' },
      { field: 'markdown', raw: { markdown: '# Res' }, json: '{"markdown":"# Res"}' },
      { field: 'output', raw: { output: 'command output' }, json: '{"output":"command output"}' },
      { field: 'text', raw: { text: 'v' }, json: '{"text":"v"}' },
      { field: 'result', raw: { result: 'v' }, json: '{"result":"v"}' },
    ];

    it.each(singleFieldCases)(
      'extracts text from $field field in raw objects and JSON strings (B2)',
      ({ raw, json }) => {
        const expected = Object.values(raw)[0];
        const rawResult = extractResultContent(raw);
        expect(rawResult.text).toBe(expected);
        expect(rawResult.isStructuredObject).toBe(false);
        expect(rawResult.structuredData).toEqual(raw);

        const jsonResult = extractResultContent(json);
        expect(jsonResult.text).toBe(expected);
        expect(jsonResult.isStructuredObject).toBe(false);
        expect(jsonResult.structuredData).toEqual(raw);
      },
    );

    it('extracts MCP content arrays from raw objects (B1)', () => {
      const rawObject = {
        content: [
          { type: 'text', text: 'part 1' },
          { type: 'text', text: 'part 2' },
        ],
      };
      const result = extractResultContent(rawObject);
      expect(result.text).toBe('part 1\n\npart 2');
      expect(result.isStructuredObject).toBe(false);
      expect(result.structuredData).toEqual(rawObject);
    });

    it('extracts MCP content arrays from JSON strings (B1)', () => {
      const jsonString = JSON.stringify({
        content: [
          { type: 'text', text: 'part 1' },
          { type: 'text', text: 'part 2' },
        ],
      });
      const result = extractResultContent(jsonString);
      expect(result.text).toBe('part 1\n\npart 2');
      expect(result.isStructuredObject).toBe(false);
      expect(result.structuredData).toEqual(JSON.parse(jsonString));
    });

    it('extracts MCP content arrays mixing plain strings and text objects (B1)', () => {
      const rawMixed = {
        content: [
          'plain part 1',
          { type: 'text', text: 'part 2' },
        ],
      };
      const result = extractResultContent(rawMixed);
      expect(result.text).toBe('plain part 1\n\npart 2');
      expect(result.isStructuredObject).toBe(false);

      const jsonMixed = JSON.stringify(rawMixed);
      const jsonResult = extractResultContent(jsonMixed);
      expect(jsonResult.text).toBe('plain part 1\n\npart 2');
      expect(jsonResult.isStructuredObject).toBe(false);
    });

    it('converts primitive values to text and structuredData fallback (B2)', () => {
      expect(extractResultContent(42)).toEqual({
        text: '42',
        isStructuredObject: false,
        structuredData: { result: 42 },
      });
      expect(extractResultContent(0)).toEqual({
        text: '0',
        isStructuredObject: false,
        structuredData: { result: 0 },
      });
      expect(extractResultContent(true)).toEqual({
        text: 'true',
        isStructuredObject: false,
        structuredData: { result: true },
      });
      expect(extractResultContent(false)).toEqual({
        text: 'false',
        isStructuredObject: false,
        structuredData: { result: false },
      });
    });

    it('identifies structured objects without text fields for JsonView rendering', () => {
      const obj = { count: 10, items: ['a', 'b'] };
      expect(extractResultContent(obj)).toEqual({
        text: null,
        isStructuredObject: true,
        structuredData: obj,
      });
      expect(extractResultContent(JSON.stringify(obj))).toEqual({
        text: null,
        isStructuredObject: true,
        structuredData: obj,
      });
    });
  });
});
