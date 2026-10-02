import { createContext } from 'react';

export type CompactionPhase = 'idle' | 'compacting' | 'failed';

export interface CompactionControl {
  phase: CompactionPhase;
  compactionId?: string;
}

/**
 * The compaction phase once a prompt run ends.
 *
 * Automatic compaction carries no id and runs inside the turn. The worker
 * finishes it before it writes the `TurnEnd` that ends the run, so the run's
 * end means the compaction is over even when the lossy `CompactingCompleted`
 * event never arrived. A manual compaction has an id, runs outside any prompt
 * run, and keeps its phase until its own result arrives.
 */
export function compactionAfterRunTerminal(control: CompactionControl): CompactionControl {
  return control.phase === 'compacting' && !control.compactionId ? { phase: 'idle' } : control;
}

export const CompactionContext = createContext<CompactionControl>({
  phase: 'idle',
});
