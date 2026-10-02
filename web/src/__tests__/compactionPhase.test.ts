import { describe, it, expect } from 'vitest';
import { compactionAfterRunTerminal } from '../CompactionContext';

describe('compactionAfterRunTerminal', () => {
  it('settles an automatic compaction when the run ends', () => {
    expect(compactionAfterRunTerminal({ phase: 'compacting' })).toEqual({ phase: 'idle' });
  });

  it('keeps a manual compaction until its own result arrives', () => {
    const manual = { phase: 'compacting' as const, compactionId: 'compact-1' };
    expect(compactionAfterRunTerminal(manual)).toBe(manual);
  });

  it('leaves idle and failed phases alone', () => {
    const idle = { phase: 'idle' as const };
    const failed = { phase: 'failed' as const };
    expect(compactionAfterRunTerminal(idle)).toBe(idle);
    expect(compactionAfterRunTerminal(failed)).toBe(failed);
  });
});
