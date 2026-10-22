import { createContext, useContext } from 'react';

export interface ArchivedHistoryTarget {
  agent: string;
  session: string;
  beforeCompaction: number;
  title?: string;
}

export interface ArchivedHistoryContextValue {
  openHistory: (target: ArchivedHistoryTarget) => void;
}

export const ArchivedHistoryContext = createContext<ArchivedHistoryContextValue | null>(null);

export function useArchivedHistory(): ArchivedHistoryContextValue | null {
  return useContext(ArchivedHistoryContext);
}
