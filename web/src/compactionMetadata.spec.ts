import { describe, expect, it } from 'vitest';
import { parseCompactionMetadata } from './compactionMetadata';

const metadata = {
  message_id: 'compaction-marker',
  compaction_seq: 5,
  summary: '',
  from_seq: 1,
  to_seq: 4,
};

describe('parseCompactionMetadata field validation', () => {
  it.each([
    ['message_id', ''], ['message_id', '  '], ['message_id', 5],
    ['compaction_seq', -1], ['compaction_seq', 1.5], ['compaction_seq', '5'],
    ['compaction_seq', Number.MAX_SAFE_INTEGER + 1],
    ['summary', null], ['from_seq', '1'], ['from_seq', Infinity], ['to_seq', NaN],
  ])('rejects invalid %s: %s', (key, value) => {
    expect(parseCompactionMetadata({ ...metadata, [key]: value })).toBeNull();
  });

  it.each(Object.keys(metadata))('requires %s', (key) => {
    const value: Record<string, unknown> = { ...metadata };
    delete value[key];
    expect(parseCompactionMetadata(value)).toBeNull();
  });

  it.each([null, undefined, false, 5, 'metadata', []])('rejects non-record %s', (value) => {
    expect(parseCompactionMetadata(value)).toBeNull();
  });

  it('ignores malformed optional fields and unknown properties', () => {
    expect(parseCompactionMetadata({
      ...metadata, kept_count: '0', earlier_compaction: 1.5, extra: 'ignored',
    })).toEqual(metadata);
  });

  it('keeps zero counts, optional sequences, and empty summaries', () => {
    const value = { ...metadata, compaction_seq: 0, kept_count: 0, earlier_compaction: 0 };
    expect(parseCompactionMetadata(value)).toEqual(value);
  });
});
