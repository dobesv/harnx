import { createContext } from 'react';
import type { CancelResult } from './types';

export interface CancellationControl {
  phase: 'idle' | 'requesting' | 'stopping' | 'unconfirmed' | 'failed';
  stop: () => Promise<void>;
  observe: (receipt: CancelResult) => void;
}

export const CancellationContext = createContext<CancellationControl>({
  phase: 'idle', stop: async () => {}, observe: () => {},
});
