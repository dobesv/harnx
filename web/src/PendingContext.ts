import { createContext } from 'react';

export const PendingContext = createContext<{
  pendingText: string | null;
  setPendingText: (t: string | null) => void;
  statusText: string | null;
  setStatusText: (t: string | null) => void;
}>({ 
  pendingText: null, 
  setPendingText: () => {},
  statusText: null,
  setStatusText: () => {} 
});
