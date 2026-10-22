import { createContext, useContext } from 'react';
import type { CompactionMetadata } from './compactionMetadata';

export interface CompactionMetadataContextValue {
  metadata: CompactionMetadata | null;
  agent?: string;
  session?: string;
}

export type CompactionMetadataContextType = CompactionMetadataContextValue | CompactionMetadata | null;

export const CompactionMetadataContext = createContext<CompactionMetadataContextType>(null);

export function useCompactionMetadata(): CompactionMetadataContextValue | null {
  const context = useContext(CompactionMetadataContext);
  if (!context) return null;
  // If passed directly as CompactionMetadata
  if ('message_id' in context) {
    return { metadata: context };
  }
  return context;
}
