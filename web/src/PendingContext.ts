import { createContext } from 'react';

export const PendingContext = createContext<{
  statusText: string | null;
  setStatusText: (t: string | null) => void;
  errorText: string | null;
  setErrorText: (t: string | null) => void;
}>({ 
  statusText: null,
  setStatusText: () => {},
  errorText: null,
  setErrorText: () => {} 
});
