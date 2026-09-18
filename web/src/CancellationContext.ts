import { createContext } from 'react';

export interface CancellationControl {
  // `requesting` lasts only as long as the append: acceptance returns the
  // composer, and `failed` means the log never took the interrupt.
  phase: 'idle' | 'requesting' | 'failed';
  stop: () => Promise<void>;
}

export const CancellationContext = createContext<CancellationControl>({
  phase: 'idle', stop: async () => {},
});
