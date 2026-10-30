import { describe, it, expect } from 'vitest';
import {
  splitThinkBlocks,
  THINK_TAG_PAIRS,
  hasThinkTags,
  hasUnclosedThinkOpener,
  resolveThinkSegments,
} from '../lib/thinkBlocks';

describe('splitThinkBlocks', () => {
  it('handles text with no tags', () => {
    expect(splitThinkBlocks('plain text without thinking')).toEqual([
      { type: 'text', text: 'plain text without thinking' },
    ]);
  });

  it('handles empty string', () => {
    expect(splitThinkBlocks('')).toEqual([]);
  });

  it('handles leading think block (harnx convention)', () => {
    const input = '<think>\nreasoning here\n</think>\n\nfinal answer';
    expect(splitThinkBlocks(input)).toEqual([
      { type: 'thought', text: 'reasoning here' },
      { type: 'text', text: '\n\nfinal answer' },
    ]);
  });

  it('handles middle think block', () => {
    const input = 'before <think>inner thought</think> after';
    expect(splitThinkBlocks(input)).toEqual([
      { type: 'text', text: 'before ' },
      { type: 'thought', text: 'inner thought' },
      { type: 'text', text: ' after' },
    ]);
  });

  it('handles multiple think blocks', () => {
    const input = '<think>first thought</think>middle<think>second thought</think>';
    expect(splitThinkBlocks(input)).toEqual([
      { type: 'thought', text: 'first thought' },
      { type: 'text', text: 'middle' },
      { type: 'thought', text: 'second thought' },
    ]);
  });

  it('handles <thinking> Claude-style tags', () => {
    const input = '<thinking>\nclaude thoughts\n</thinking>\nanswer';
    expect(splitThinkBlocks(input)).toEqual([
      { type: 'thought', text: 'claude thoughts' },
      { type: 'text', text: '\nanswer' },
    ]);
  });

  it('handles unclosed trailing tag as thought to the end and preserves trailing newlines', () => {
    for (const { open } of THINK_TAG_PAIRS) {
      const input = `answer${open}\nunfinished\n`;
      expect(splitThinkBlocks(input)).toEqual([
        { type: 'text', text: 'answer' },
        { type: 'thought', text: 'unfinished\n' },
      ]);
    }
  });

  it('preserves multiple trailing newlines in unclosed thought blocks', () => {
    expect(splitThinkBlocks('<think>\nline 1\nline 2\n\n\n')).toEqual([
      { type: 'thought', text: 'line 1\nline 2\n\n\n' },
    ]);
  });

  it('handles mismatched closers (closer treated as text)', () => {
    expect(splitThinkBlocks('<think>one</thinking>two</think>answer')).toEqual([
      { type: 'thought', text: 'one</thinking>two' },
      { type: 'text', text: 'answer' },
    ]);
    expect(splitThinkBlocks('<thinking>one</think>two</thinking>answer')).toEqual([
      { type: 'thought', text: 'one</think>two' },
      { type: 'text', text: 'answer' },
    ]);
    expect(splitThinkBlocks('<think>one</thinking>answer')).toEqual([
      { type: 'thought', text: 'one</thinking>answer' },
    ]);
    expect(splitThinkBlocks('before</think>after</thinking>')).toEqual([
      { type: 'text', text: 'before</think>after</thinking>' },
    ]);
  });

  it('drops empty thought blocks', () => {
    for (const input of [
      '<think></think>',
      '<think>\n</think>',
      '<thinking>\n\n</thinking>',
      '<think>',
      '<thinking>\n',
    ]) {
      expect(splitThinkBlocks(input)).toEqual([]);
    }
    expect(splitThinkBlocks('before<think></think>after')).toEqual([
      { type: 'text', text: 'before' },
      { type: 'text', text: 'after' },
    ]);
  });

  it('trims only one newline at each boundary', () => {
    expect(splitThinkBlocks('<think>\n\n reasoning \n\n</think>\n')).toEqual([
      { type: 'thought', text: '\n reasoning \n' },
      { type: 'text', text: '\n' },
    ]);
    expect(splitThinkBlocks('<think> </think>')).toEqual([
      { type: 'thought', text: ' ' },
    ]);
  });

  it('is case sensitive', () => {
    expect(splitThinkBlocks('<Think>reasoning</Think>')).toEqual([
      { type: 'text', text: '<Think>reasoning</Think>' },
    ]);
  });

  it('does not treat tags inside code fences specially', () => {
    expect(splitThinkBlocks('```text\n<think>reasoning</think>\n```')).toEqual([
      { type: 'text', text: '```text\n' },
      { type: 'thought', text: 'reasoning' },
      { type: 'text', text: '\n```' },
    ]);
  });

  it('does not nest', () => {
    expect(splitThinkBlocks('<think>one<thinking>two</thinking></think>answer')).toEqual([
      { type: 'thought', text: 'one<thinking>two</thinking>' },
      { type: 'text', text: 'answer' },
    ]);
  });
});

describe('hasThinkTags', () => {
  it('returns false for plain text', () => {
    expect(hasThinkTags('plain text')).toBe(false);
  });

  it('returns true when opening or closing think tags exist', () => {
    expect(hasThinkTags('<think>')).toBe(true);
    expect(hasThinkTags('</think>')).toBe(true);
    expect(hasThinkTags('<thinking>')).toBe(true);
    expect(hasThinkTags('</thinking>')).toBe(true);
    expect(hasThinkTags('<think></think>answer')).toBe(true);
  });
});

describe('hasUnclosedThinkOpener', () => {
  it('identifies unclosed openers', () => {
    expect(hasUnclosedThinkOpener('<think>')).toBe(true);
    expect(hasUnclosedThinkOpener('<think>\n')).toBe(true);
    expect(hasUnclosedThinkOpener('before<think>')).toBe(true);
    expect(hasUnclosedThinkOpener('<thinking>content')).toBe(true);
    expect(hasUnclosedThinkOpener('<think>first</think>second<thinking>')).toBe(true);
  });

  it('returns false when all openers are closed or no tags present', () => {
    expect(hasUnclosedThinkOpener('plain text')).toBe(false);
    expect(hasUnclosedThinkOpener('<think></think>')).toBe(false);
    expect(hasUnclosedThinkOpener('<think>content</think>')).toBe(false);
    expect(hasUnclosedThinkOpener('<think></think>answer')).toBe(false);
    expect(hasUnclosedThinkOpener('before</think>after')).toBe(false);
  });
});

describe('resolveThinkSegments', () => {
  it('Case 1: emits empty thought segment for streaming unclosed opener so UI shows thinking block immediately', () => {
    expect(resolveThinkSegments('<think>', { isStreaming: true })).toEqual([
      { type: 'thought', text: '' },
    ]);
    expect(resolveThinkSegments('<think>\n', { isStreaming: true })).toEqual([
      { type: 'thought', text: '' },
    ]);
    expect(resolveThinkSegments('prefix <think>', { isStreaming: true })).toEqual([
      { type: 'text', text: 'prefix ' },
      { type: 'thought', text: '' },
    ]);
  });

  it('does not duplicate thought segment when streaming chunk already has content', () => {
    expect(resolveThinkSegments('<think>reasoning', { isStreaming: true })).toEqual([
      { type: 'thought', text: 'reasoning' },
    ]);
  });

  it('emits empty thought segment for adjacent streaming opener following closed thought', () => {
    expect(resolveThinkSegments('<think>first</think><thinking>', { isStreaming: true })).toEqual([
      { type: 'thought', text: 'first' },
      { type: 'thought', text: '' },
    ]);
  });

  it('Case 2: keeps text segments and drops empty thought when not streaming', () => {
    expect(resolveThinkSegments('<think></think>answer', { isStreaming: false })).toEqual([
      { type: 'text', text: 'answer' },
    ]);
    expect(resolveThinkSegments('<thinking></thinking>', { isStreaming: false })).toEqual([]);
    expect(resolveThinkSegments('<think>', { isStreaming: false })).toEqual([]);
  });
});
