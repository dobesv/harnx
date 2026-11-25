import { createContext, useContext } from 'react';

export const HistoryReasoningSupportContext = createContext<boolean>(false);

export function useHistoryReasoningSupport(): boolean {
  return useContext(HistoryReasoningSupportContext);
}
