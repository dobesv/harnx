import { createContext, useContext } from 'react';
import type { CompactionMetadata } from './compactionMetadata';

export const CompactionMetadataContext = createContext<CompactionMetadata | null>(null);

export function useCompactionMetadata(): CompactionMetadata | null {
  return useContext(CompactionMetadataContext);
}
