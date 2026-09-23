import { createContext } from 'react';

export type CompactionPhase = 'idle' | 'compacting' | 'failed';

export interface CompactionControl {
  phase: CompactionPhase;
  compactionId?: string;
}

export const CompactionContext = createContext<CompactionControl>({
  phase: 'idle',
});
