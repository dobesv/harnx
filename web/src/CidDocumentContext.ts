import { createContext, useContext } from 'react';

export interface CidDocumentTarget {
  cid: string;
  title?: string;
}

export interface CidDocumentContextValue {
  openDocument: (target: CidDocumentTarget) => void;
}

export const CidDocumentContext = createContext<CidDocumentContextValue | null>(null);

export function useCidDocument(): CidDocumentContextValue | null {
  return useContext(CidDocumentContext);
}
